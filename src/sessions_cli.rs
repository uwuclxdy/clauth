//! `clauth sessions [--json] [--tokens]`, `clauth resume <id|latest>
//! [--profile <name>]`, and `clauth info <id|latest>` — the CLI surface over the
//! session index ([`crate::sessions`]). The index owns the heavy work (transcript
//! walk, preview redaction, token/cost annotation, owner stamping); this module
//! only flattens it, renders it, and drives the account-aware resume spawn.
//!
//! # What each command reads
//! Only `sessions` browses, so only `sessions` builds the index. `resume` and
//! `info` want one row, and take [`crate::sessions::find_session`] /
//! [`crate::sessions::newest_session`] instead: a filename-and-mtime walk, then
//! the head of the single transcript they resolved to. Those two read the shared
//! store only, so a target the index would have found in a live isolated runtime
//! is reported against [`crate::sessions::live_isolated_holds`] (the same tier-1
//! walk) rather than called missing — see [`Resolved`]. The token and cost
//! figures are a third tier above even the index — a full read of every
//! transcript — so `sessions` leaves them blank until `--tokens` asks.
//!
//! Over the maintainer's own 12k-session, 5.4 GB store: `clauth info latest`
//! 11.3 s → 44 ms, `clauth sessions` 20.7 s → 11.3 s, and `--tokens` reproduces
//! the old listing byte for byte.
//!
//! # Exit codes (the `clauth sessions` scripting contract)
//! - `0` success.
//! - `1` a genuine error, INCLUDING "no sessions found".
//! - `2` a usage error (bad flag/args).
//!
//! `1` vs `2` is carried by [`crate::UsageError`] and mapped in
//! [`crate::exit_code`]: a `sessions`/`resume`/`info` dispatch arm returns a
//! `UsageError` for a malformed invocation, and any other `Err` (an empty index
//! included) maps to `1`.

use std::io::IsTerminal as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::out::{out, outln};
use crate::profile::{AppConfig, load_config};
use crate::runtime::Isolation;
use crate::sessions::{IsolatedHold, SessionInfo, SessionRef, WorkspaceGroup};

/// `clauth sessions [--json] [--tokens]` — the full inventory, newest-first.
/// Both a TTY and a pipe print a table (the `--json` flag, not the tty, selects
/// machine output; this is deliberately NOT showagent's pipe-prints-different
/// behavior). An empty index is exit 1 ("no sessions found") on both paths, per
/// the scripting contract above.
pub(crate) fn run_sessions(json: bool, tokens: bool) -> Result<()> {
    let groups = build_listing(tokens);

    let flat = flatten_newest_first(&groups);
    if flat.is_empty() {
        anyhow::bail!("no sessions found");
    }
    if json {
        outln!("{}", sessions_json(&flat));
    } else {
        emit_sessions_table(&groups, tokens);
    }
    Ok(())
}

/// The listing's data, with the token/cost annotation left off unless asked for.
///
/// That annotation reads every transcript in the store IN FULL — a tier above
/// the index's own bounded head+tail reads, and the reason it is opt-in: a
/// listing should not cost a multi-gigabyte parse to show ids and previews.
/// Skipped, `tokens`/`cost` stay `None`, which the table renders as no columns
/// at all and `--json` as `null` — the same `null` a session with no
/// token-bearing row gets, so a consumer that wants the figures asks for them
/// rather than inferring anything from a blank.
fn build_listing(tokens: bool) -> Vec<WorkspaceGroup> {
    let mut groups = crate::sessions::build_index();
    if tokens {
        // A cold price cache prices nothing (blank cost), never blocks the listing.
        let price = crate::pricing::load_cached();
        crate::sessions::annotate_all(&mut groups, price.as_ref());
    }
    crate::sessions::annotate_owners(&mut groups);
    groups
}

/// `clauth resume <id|latest> [--profile <name>]` — resume a session through the
/// existing `clauth start` spawn path (runtime prep, signal forwarding, lifetime
/// guard), with `--resume <id>` injected and the session's recorded workspace as
/// the child cwd. Never a second spawn implementation. `latest` = the newest
/// session `clauth sessions` would list first; any other value is an exact id
/// match. Either can name a session a live isolated run holds, which is refused
/// by name rather than resumed or silently swapped for another.
pub(crate) fn run_resume(target: &str, profile_flag: Option<&str>) -> Result<()> {
    crate::platform::init();
    crate::runtime::gc_stale_runtimes();
    let config = load_config()?;

    let session = match resolve_session(target) {
        Resolved::Ready(session) => session,
        Resolved::Held(hold) => return Err(held_refusal(target, &hold)),
        Resolved::Missing => anyhow::bail!("no session found for '{target}'"),
    };

    // Resume must land in the recorded workspace, else `--resume` would run in
    // the wrong dir (or fail to find the transcript). Refuse rather than spawn.
    let workspace = session.workspace().ok_or_else(|| {
        anyhow::anyhow!(
            "can't resume '{}': no workspace recorded for it",
            session.id
        )
    })?;
    if !workspace.is_dir() {
        anyhow::bail!(
            "can't resume '{}': workspace '{}' no longer exists",
            session.id,
            workspace.display()
        );
    }

    // The owner drives the interactive profile default.
    let last_ran = crate::sessions::owner_of(&session.id);
    let active = config.state.active_profile.as_deref().unwrap_or_default();
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (default_profile, should_prompt) =
        resume_profile_choice(profile_flag, is_tty, last_ran.as_deref(), active);
    let chosen = if should_prompt {
        prompt_profile(&config, &default_profile)?
    } else {
        default_profile
    };

    let canonical = resolve_profile_name(&config, &chosen)?;

    let resume_args = vec!["--resume".to_string(), session.id];
    // Shared isolation: a resume adopts the chosen account against the shared
    // store, the same lifecycle a bare `clauth start <name>` uses. A resume never
    // opts into the fallback chain — there is no `--with-fallback` on this
    // surface to ask for it.
    crate::start::run(
        &config,
        &canonical,
        &resume_args,
        Isolation::Shared,
        Some(&workspace),
        false,
    )
}

/// `clauth info <id|latest>` — print the exact `clauth resume` command, the
/// workspace, and the on-disk storage path. Never launches anything.
pub(crate) fn run_info(target: &str) -> Result<()> {
    let (session, held_by) = match resolve_session(target) {
        Resolved::Ready(session) => (session, None),
        // `info` launches nothing, so a held session is reportable where it is
        // not resumable: its storage path is the one thing that says where the
        // transcript actually lives, and nothing else on any surface prints it.
        Resolved::Held(hold) => (hold.session, Some(hold.profile)),
        Resolved::Missing => anyhow::bail!("no session found for '{target}'"),
    };
    outln!("{}", info_lines(&session, held_by.as_deref()));
    Ok(())
}

/// The three lines `clauth info` prints. Pure, so both variants are assertable
/// without capturing stdout. A held session gets no resume command: printing one
/// that Claude Code would answer `No conversation found` for is worse than
/// saying why there isn't one.
fn info_lines(session: &SessionRef, held_by: Option<&str>) -> String {
    let resume = match held_by {
        Some(profile) => {
            format!("unavailable while a live isolated run under '{profile}' holds this session")
        }
        None => format!("clauth resume {}", session.id),
    };
    format!(
        "resume:    {resume}\nworkspace: {}\nstorage:   {}",
        session.workspace().unwrap_or_default().display(),
        session.path.display(),
    )
}

/// Pick the resume profile default and whether to prompt for it, across the four
/// branches:
/// 1. explicit `--profile` → that profile, forced (never prompt).
/// 2. piped/non-TTY, no flag → the active profile, forced (can't prompt).
/// 3. TTY, no flag, known last-ran → prompt, defaulting to the last-ran profile.
/// 4. TTY, no flag, unknown last-ran → prompt, defaulting to the active profile.
///
/// Pure and returns `(default_profile, should_prompt)` so the four branches are
/// unit-testable without a terminal.
fn resume_profile_choice(
    flag: Option<&str>,
    is_tty: bool,
    last_ran: Option<&str>,
    active: &str,
) -> (String, bool) {
    if let Some(explicit) = flag {
        return (explicit.to_string(), false);
    }
    if !is_tty {
        return (active.to_string(), false);
    }
    match last_ran {
        Some(p) => (p.to_string(), true),
        None => (active.to_string(), true),
    }
}

/// Resolve a chosen profile name to its canonical spelling, or an error listing
/// the available names — mirrors `main::resolve_or_bail`.
fn resolve_profile_name(config: &AppConfig, chosen: &str) -> Result<crate::profile::ProfileName> {
    config
        .canonical_name(chosen)
        .map(crate::profile::ProfileName::from)
        .ok_or_else(|| {
            let available = config.names().join(", ");
            anyhow::anyhow!("profile '{chosen}' not found\navailable: {available}")
        })
}

/// The candidate list [`prompt_profile`] offers, plus the resolved default:
/// every enabled profile name ([`AppConfig::enabled_profiles`], the same view
/// `which`/`status` read), with `default` swapped for the first enabled name
/// when the caller's default is itself disabled — a stale `last_ran_profile`
/// that's since been disabled must not show as the bracketed default for a
/// name that isn't even listed. Pure so the disabled-exclusion is
/// unit-testable without a terminal.
fn resume_candidates<'a>(config: &'a AppConfig, default: &'a str) -> (Vec<&'a str>, &'a str) {
    let enabled: Vec<&str> = config.enabled_profiles().map(|p| p.name.as_str()).collect();
    let resolved = if enabled.contains(&default) {
        default
    } else {
        enabled.first().copied().unwrap_or(default)
    };
    (enabled, resolved)
}

/// Interactive profile prompt: list the enabled profiles (the default
/// marked), read a line, and take the default on empty input. TTY-only —
/// reached only when [`resume_profile_choice`] returns `should_prompt`. An
/// explicit `--profile <disabled>` skips this prompt entirely and is still
/// caught by `start::run`'s authoritative refusal.
fn prompt_profile(config: &AppConfig, default: &str) -> Result<String> {
    let (enabled, default) = resume_candidates(config, default);
    outln!("resume under which account?");
    for name in enabled.iter().copied() {
        let marker = if name == default { "  (default)" } else { "" };
        outln!("  {name}{marker}");
    }
    out!("profile [{default}]: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let picked = line.trim();
    Ok(if picked.is_empty() {
        default.to_string()
    } else {
        picked.to_string()
    })
}

/// Flatten every group's sessions into one newest-first list. Groups are already
/// newest-first, but a flat cross-workspace order needs the same key
/// (`updated` desc, id asc) as [`crate::sessions`]'s within-group sort.
fn flatten_newest_first(groups: &[WorkspaceGroup]) -> Vec<&SessionInfo> {
    let mut all: Vec<&SessionInfo> = groups.iter().flat_map(|g| g.sessions.iter()).collect();
    all.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    all
}

/// What a `<id|latest>` target resolved to. "Not in the shared store" is not
/// "not there": `clauth sessions` browses live isolated stores too, so a target
/// naming one of those is a real session that a resume simply cannot reach yet,
/// and saying "no session found" for it would be false.
enum Resolved {
    /// A session a resume can reach.
    Ready(SessionRef),
    /// A live isolated run holds it.
    Held(IsolatedHold),
    /// No session of that name anywhere.
    Missing,
}

/// Resolve `latest` to the newest session, or any other value to an exact id
/// match.
///
/// Both forms are the targeted lookup, never [`crate::sessions::build_index`]:
/// `resume` and `info` each use one row, and building the whole index to find it
/// reads every transcript in the store twice over. `latest` keeps the index's
/// own newest-first ordering over the sessions a resume can reach, so the two
/// surfaces agree except where the listing's first row is a nested transcript
/// Claude Code will not open.
fn resolve_session(target: &str) -> Resolved {
    let found = if target == "latest" {
        crate::sessions::newest_session()
    } else {
        crate::sessions::find_session(target)
    };
    match shadowing_hold(target, found.as_ref()) {
        Some(hold) => Resolved::Held(hold),
        None => found.map_or(Resolved::Missing, Resolved::Ready),
    }
}

/// The live isolated transcript that makes the shared store's answer the wrong
/// one to act on.
///
/// For an exact id: the run holding that id, asked only once the shared store
/// has come up empty. A rescue in flight can leave one id in both stores, and
/// there the shared copy is the reachable one, so a hit is never second-guessed.
///
/// For `latest`: a transcript strictly newer than the newest reachable session.
/// Without this the newest session on the machine drops out of the search and
/// `latest` quietly names the second newest — a session the operator never
/// asked for, spending an account window on the wrong conversation. An exact
/// mtime tie leaves the reachable session the answer. Only the isolated
/// transcripts a rescue could make resumable count here, matching what
/// [`crate::sessions::newest_session`] ranges over; a nested one is never
/// anybody's `latest`, in either store.
fn shadowing_hold(target: &str, found: Option<&SessionRef>) -> Option<IsolatedHold> {
    if target != "latest" {
        if found.is_some() {
            return None;
        }
        return crate::sessions::live_isolated_holds()
            .into_iter()
            .find(|h| h.session.id == target);
    }
    let newest = crate::sessions::live_isolated_top_level_holds()
        .into_iter()
        .max_by(|a, b| {
            a.session
                .updated
                .cmp(&b.session.updated)
                .then_with(|| a.session.path.cmp(&b.session.path))
        })?;
    found
        .is_none_or(|f| newest.session.updated > f.updated)
        .then_some(newest)
}

/// The refusal for a target a live isolated run holds. Names the run's profile
/// and how the session becomes reachable, since "wait for it" is only actionable
/// if the operator knows the run ending is what moves it.
fn held_refusal(target: &str, hold: &IsolatedHold) -> anyhow::Error {
    let what = if target == "latest" {
        format!("the newest session ('{}')", hold.session.id)
    } else {
        format!("'{}'", hold.session.id)
    };
    anyhow::anyhow!(
        "can't resume '{target}': {what} belongs to a live isolated run under profile '{}', \
         whose store a resume can't read\n\
         it moves into the shared store when that run ends",
        hold.profile,
    )
}

/// The stable `clauth sessions --json` array (newest-first). Documented fields
/// only: `id`, `last_ran_profile`, `workspace`, `updated`, `first_message`,
/// `last_message`, `tokens`, `cost`. Absent `tokens`/`cost` serialize to JSON
/// `null` (never `0`) — and without `--tokens` nothing asked for them, so every
/// row's pair is `null`. `updated` is ISO-8601 UTC
/// (`YYYY-MM-DDTHH:MM:SS+00:00`), matching the rest of clauth's timestamps —
/// and deliberately NOT the human table's shape, which renders the same
/// instant in local wall clock with a relative age (the 2026-08-22
/// prose-stamp ruling).
fn sessions_json(sessions: &[&SessionInfo]) -> serde_json::Value {
    serde_json::Value::Array(sessions.iter().map(|s| session_json_row(s)).collect())
}

fn session_json_row(s: &SessionInfo) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "last_ran_profile": s.last_ran_profile,
        "workspace": s.workspace,
        "updated": updated_iso(s.updated),
        "first_message": s.first_message,
        "last_message": s.last_message,
        "tokens": s.tokens,
        "cost": s.cost,
    })
}

/// Human table: a workspace header per group, then one row per session. The
/// index already redacted the previews, so nothing is masked here. The token and
/// cost columns appear only under `--tokens`; two permanently blank columns
/// would otherwise eat the width the previews want.
fn emit_sessions_table(groups: &[WorkspaceGroup], tokens: bool) {
    // One clock read for the whole table: every row's age is relative to the
    // same instant, and `session_row` stays pure (its `now` is a parameter,
    // never a read hidden inside).
    let now = SystemTime::now();
    for group in groups {
        let ws = if group.workspace.is_empty() {
            "(unknown workspace)"
        } else {
            &group.workspace
        };
        outln!("{ws}");
        for s in &group.sessions {
            outln!("{}", session_row(s, tokens, now));
        }
    }
}

/// One session's table row. Pure, so which columns a flag puts in it is
/// assertable without capturing stdout: `now` arrives from the caller, never
/// a clock read hidden inside. The `updated` cell is the 2026-08-22
/// prose-stamp ruling's shape — LOCAL wall clock (`YYYY-MM-DD HH:MM:SS`)
/// paired with its relative age, `2026-08-28 14:03:11 · 3h 12m ago`; the
/// machine ISO shape lives on in the `--json` row only. The token total and
/// its cost are blank when the annotation found none — never `0`, which
/// would read as a real figure — and absent entirely when `tokens` never
/// asked for them.
fn session_row(s: &SessionInfo, tokens: bool, now: SystemTime) -> String {
    let usage = if tokens {
        format!(
            "  {tokens:>10}  {cost:>8}",
            tokens = s.tokens.map(|t| t.to_string()).unwrap_or_default(),
            cost = s.cost.map(|c| format!("${c:.2}")).unwrap_or_default(),
        )
    } else {
        String::new()
    };
    let secs = s
        .updated
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The None arm is unreachable for a filesystem mtime (chrono's range
    // dwarfs any OS's); the dash is the table's no-data glyph, never a UTC
    // fallback — a bare stamp reads as local.
    let updated = crate::format::local_stamp(secs).unwrap_or_else(|| "-".to_string());
    let age_secs = now
        .duration_since(s.updated)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // `humanize_duration` spells ≤0 `now`, so the ` ago` pairing must skip
    // the non-positive ages: a sub-second-fresh file or a future mtime would
    // render `now ago`.
    let age = if age_secs <= 0 {
        "now".to_string()
    } else {
        format!("{} ago", crate::usage::humanize_duration(age_secs))
    };
    format!(
        "  {id:<8}  {profile:<12}  {updated} · {age}{usage}  {preview}",
        id = short_id(&s.id),
        profile = s.last_ran_profile.as_deref().unwrap_or("-"),
        preview = preview_pair(s),
    )
}

/// The first block of a uuid session id, enough to eyeball in the table (the
/// full id is what `clauth resume`/`info` take). A non-uuid stem shows whole.
fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// `first | last` message preview, each bounded so a long line can't blow the
/// row width. Already-redacted text, so re-truncation is safe.
fn preview_pair(s: &SessionInfo) -> String {
    let first = crate::format::truncate(s.first_message.as_deref().unwrap_or(""), 50);
    let last = crate::format::truncate(s.last_message.as_deref().unwrap_or(""), 50);
    match (first.is_empty(), last.is_empty()) {
        (true, true) => String::new(),
        (false, true) => first,
        (true, false) => last,
        (false, false) => format!("{first} | {last}"),
    }
}

/// The `--json` row's `updated` cell: a file mtime as ISO-8601 UTC, reusing
/// clauth's shared formatter. Deliberately the machine shape — the human
/// table renders the same instant in local wall clock with a relative age.
/// A pre-epoch time clamps to epoch 0.
fn updated_iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::usage::epoch_secs_to_iso(secs)
}

#[cfg(test)]
#[path = "../tests/inline/sessions_cli.rs"]
mod tests;
