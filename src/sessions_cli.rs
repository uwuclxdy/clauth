//! `clauth sessions [--json] [--tokens] [--live]`, `clauth resume <id|latest>
//! [--profile <name>]`, `clauth info <id|latest>`, and the two-name form of
//! `clauth switch <sid> <profile>` (one name is the global account switch,
//! `main::cmd_switch`) — the CLI surface over the session index
//! ([`crate::sessions`]). The index owns the heavy work (transcript walk,
//! preview redaction, token/cost annotation, owner stamping); this module
//! only flattens it, renders it, and drives the account-aware resume spawn.
//! `switch` and `sessions --live` are the exceptions: neither touches a
//! transcript — `switch` reads only the live-session registry
//! ([`crate::live_sessions`]), `--live` the registry plus Claude Code's
//! per-pid session records, and `switch --list` the same two.
//!
//! # What each command reads
//! Only the full `sessions` listing browses, so only it builds the index;
//! `--live` builds no index. `resume` and
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
//! - `1` a genuine error, INCLUDING "no sessions found", "no live sessions
//!   found", "no session clauth can switch is running", and an unlistable
//!   live-session registry.
//! - `2` a usage error (bad flag/args).
//!
//! `1` vs `2` is carried by [`crate::UsageError`] and mapped in
//! [`crate::exit_code`]: a `sessions`/`resume`/`info` dispatch arm returns a
//! `UsageError` for a malformed invocation, and any other `Err` (an empty index
//! included) maps to `1`.

use std::io::IsTerminal as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::format::{grapheme_cells, pad_cells};
use crate::out::{errln, out, outln};
use crate::profile::{AppConfig, load_config};
use crate::runtime::Isolation;
use crate::sessions::{IsolatedHold, SessionInfo, SessionRef, WorkspaceGroup};

/// `clauth sessions [--json] [--tokens] [--live]` — the full inventory,
/// newest-first, or (`--live`) the running sessions only, read from the
/// registry and Claude Code's per-pid session records with no transcript
/// opened. Both a TTY and a pipe print a table (the `--json` flag, not the tty,
/// selects machine output; this is deliberately NOT showagent's
/// pipe-prints-different behavior). An empty listing is exit 1 ("no sessions
/// found" / "no live sessions found") on both paths, per the scripting contract
/// above.
pub(crate) fn run_sessions(json: bool, tokens: bool, live: bool) -> Result<()> {
    if live {
        let rows = live_rows_newest_first(live_rows_or_bail()?);
        if rows.is_empty() {
            anyhow::bail!("no live sessions found");
        }
        if json {
            outln!("{}", live_json_of(&rows));
        } else {
            for line in live_lines_of(&rows, SystemTime::now()) {
                outln!("{line}");
            }
        }
        return Ok(());
    }

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

/// The live registry's rows, or the shared error naming the unreadable dir —
/// the prologue `sessions --live` and `switch --list` both open with.
fn live_rows_or_bail() -> Result<Vec<crate::live_sessions::LiveSession>> {
    let Some(rows) = crate::live_sessions::live_rows() else {
        let dir = crate::live_sessions::registry_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|_| "~/.clauth/live_sessions".to_string());
        anyhow::bail!("cannot list live sessions: {dir} is unreadable");
    };
    Ok(rows)
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
        None,
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

/// `clauth switch <sid> <profile>` — point a live session at another
/// profile.
///
/// Intent only, by design: the row's intended member moves through the same
/// [`crate::live_sessions::update_as_daemon`] seam the daemon's decision leg
/// writes, and this process installs no credentials. The session's own
/// executor is the safety gate — it performs the switch, or refuses it with a
/// logged reason, exactly as it treats the chain's intents.
pub(crate) fn run_switch(sid: &str, profile: &str) -> Result<()> {
    let Some(row) = crate::live_sessions::get(sid) else {
        // Arity alone chose the session form, so a name that also resolves to
        // a configured profile gets pointed at the spelling that switches the
        // global account (the same two-roster resolution `cmd_switch` takes)
        // instead of a dead end.
        let resolves_anywhere = load_config()
            .ok()
            .is_some_and(|config| config.canonical_name(sid).is_some())
            || crate::codex_profiles::CodexState::load()
                .ok()
                .is_some_and(|state| state.canonical_name(sid).is_some());
        let hint = if resolves_anywhere {
            format!("\nto switch the global account: `clauth switch {sid}`")
        } else {
            String::new()
        };
        anyhow::bail!("no live session '{sid}'\nsee `clauth switch --list`{hint}");
    };
    switch_row_checks(&row)?;

    let config = load_config()?;
    let canonical = resolve_profile_name(&config, profile)?;
    // `current_member` is None until a session's first swap, so a session that
    // never moved runs as its launch profile.
    let current = row.current_member.as_deref().unwrap_or(&row.start_profile);
    if current == canonical.as_str() {
        outln!("{}", already_on_line(sid, canonical.as_str()));
        return Ok(());
    }

    crate::live_sessions::update_as_daemon(sid, |fields| {
        fields.set_intended_member(canonical.as_str());
        // A manual move is never a credit-cap leave, so it keeps the
        // models-equal rule whatever the last daemon write recorded.
        fields.set_credit_leave(false);
    })?;
    outln!("{}", switch_receipt(sid, canonical.as_str()));
    Ok(())
}

/// The one gate every `clauth switch <sid> <profile>` target must pass before
/// a profile resolves or an intent is written — and the predicate
/// `clauth switch --list` filters on. One check, so a session `switch` refuses
/// never prints as switchable and a row the listing shows is exactly one
/// `switch` will accept. The refusal messages are the shared copy.
fn switch_row_checks(row: &crate::live_sessions::LiveSession) -> Result<()> {
    let sid = &row.session_id;
    // A codex row has no executor: codex reads auth.json once at start, so a
    // mid-session intent would stand forever as a silent no-op.
    if row.harness == crate::harness::Harness::Codex {
        anyhow::bail!("session '{sid}' is a codex session; switch is claude-only");
    }
    // The shared liveness probe: a row whose flock is gone is a stale row
    // awaiting GC, not a session to move.
    if !row.marker_is_held() {
        // A handed-off chain session has no live marker (its launcher or a
        // previous daemon released it): it waits for the next daemon, not a
        // report that it stopped. The job still naming its tree is what says
        // the hand-off stands.
        if row.follows_chain
            && !row.isolated
            && crate::runtime::chain_session_tree(
                &crate::profile::ProfileName::from(row.start_profile.as_str()),
                &row.session_id,
            )
            .ok()
            .is_some_and(|tree| crate::claude_jobs::job_trees().names(&tree))
        {
            anyhow::bail!(
                "session '{sid}' is a background session no clauth daemon holds yet\n\
                 it follows the chain once `clauth daemon` runs"
            );
        }
        anyhow::bail!(
            "session '{sid}' is no longer running\n\
             its row is reaped by the next `clauth daemon` or `clauth resume`"
        );
    }
    // A background session is fixed on its account: its tree is the account's
    // one fixed pair, shared by every Claude Code job there, so a move would
    // bill another account under a running job. Refused before any profile
    // resolution and before any intent is written — the executor refuses the
    // same shape as its last line of defence.
    if row.background {
        anyhow::bail!(
            "session '{sid}' is a background session and cannot be switched from '{}'",
            row.start_profile
        );
    }
    Ok(())
}

/// The success receipt. Pure so the copy is assertable without capturing
/// stdout; the timing sentence is the multi-session design record's own
/// wording for when a switch lands.
fn switch_receipt(sid: &str, profile: &str) -> String {
    format!(
        "clauth: pointed session '{sid}' at '{profile}'\n\
         the switch lands at the session's next request, never before it. \
         A refused move is logged and the session stays put"
    )
}

/// The no-op line for a session already running the named profile: the
/// executor treats an intent equal to the current member as the steady state,
/// so writing one would promise a move that never comes.
fn already_on_line(sid: &str, profile: &str) -> String {
    format!("clauth: session '{sid}' is already on '{profile}'")
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
            .find(|h| {
                h.session.id == target && !crate::sessions::is_workflow_journal(&h.session.path)
            });
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
        "updated": crate::sessions::updated_iso(s.updated),
        "first_message": s.first_message,
        "last_message": s.last_message,
        "tokens": s.tokens,
        "cost": s.cost,
    })
}

// ── clauth switch --list ──

/// `clauth switch --list [--json]` — the running sessions `clauth switch <sid>
/// <profile>` accepts, newest first, and nothing else. Filtered by the same
/// [`switch_row_checks`] gate `run_switch` applies, so a row shown is exactly
/// one `switch` will take. The plain `claude` sessions the listing leaves out
/// are counted and noted on stderr after the rows. Nothing switchable exits 1,
/// the same scripting contract as `sessions --live`, with the note still
/// printed before the error.
pub(crate) fn run_switch_list(json: bool) -> Result<()> {
    let live = live_rows_or_bail()?;
    let note = plain_claude_note(plain_claude_count_of(&live));
    let rows = switch_list_rows(live);
    if !rows.is_empty() {
        if json {
            outln!("{}", switch_list_json_of(&rows));
        } else {
            for line in switch_list_lines_of(&rows, SystemTime::now()) {
                outln!("{line}");
            }
        }
    }
    if let Some(note) = note {
        errln!("{note}");
    }
    if rows.is_empty() {
        anyhow::bail!("no session clauth can switch is running");
    }
    Ok(())
}

/// The roster rows the listing keeps: newest first, then only the ones
/// [`switch_row_checks`] accepts. The `switch` session-id completion filters
/// through the same call, so it offers exactly what the listing shows.
pub(crate) fn switch_list_rows(
    rows: Vec<crate::live_sessions::LiveSession>,
) -> Vec<crate::live_sessions::LiveSession> {
    live_rows_newest_first(
        rows.into_iter()
            .filter(|row| switch_row_checks(row).is_ok())
            .collect(),
    )
}

/// The stderr note naming the plain `claude` sessions the listing leaves out,
/// for a nonzero count: singular, plural, or nothing at zero.
fn plain_claude_note(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some(
            "1 claude session not listed: switch can only move sessions started with `clauth start`"
                .to_string(),
        ),
        n => Some(format!(
            "{n} claude sessions not listed: switch can only move sessions started with `clauth start`"
        )),
    }
}

/// [`plain_claude_count_of`] over a fresh registry read.
#[cfg(all(test, target_os = "linux"))]
fn plain_claude_count() -> usize {
    plain_claude_count_of(&crate::live_sessions::live_rows().unwrap_or_default())
}

/// How many running `claude` sessions in `live`'s snapshot have no live
/// registry row: Claude Code records under the global `~/.claude/sessions`
/// whose process still matches its record
/// ([`crate::session_record::process_matches_record`]), whose pid no live
/// `claude` row names, directly or through one of its first
/// [`crate::session_record::ANCESTOR_DEPTH`] ancestors, and whose conversation
/// the job store does not run on a clauth runtime tree (a
/// `clauth start -- --bg` job records into the global dir under a pid no row
/// holds, yet it is a clauth session). That match is per-platform: on linux
/// a recycled pid with a stale record never counts, on macOS liveness alone
/// is compared so it can, and elsewhere the count is always 0. An unreadable sessions dir counts 0. Only `claude` rows hold a
/// pid: a codex row's process gives no runtime `CLAUDE_CONFIG_DIR` to a
/// `claude` below it, so that `claude` records globally and is plain.
fn plain_claude_count_of(live: &[crate::live_sessions::LiveSession]) -> usize {
    let Some(dir) = crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("sessions"))
    else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let holds = |pid: u32| {
        live.iter().any(|row| {
            row.harness == crate::harness::Harness::Claude
                && (row.pid == pid || row.claude_pid == Some(pid))
        })
    };
    // A session of clauth's is held by a row through the record's own pid or
    // through one of its ancestors: a launcher whose `claude_pid` write failed,
    // or a `claude` run from a clauth session's shell.
    let listed = |pid: u32| {
        holds(pid)
            || crate::session_record::first_ancestor(pid, |ancestor| holds(ancestor).then_some(()))
                .is_some()
    };
    // Read once per call; only a record the store ties to one of clauth's
    // runtime trees is excluded, the set GC walks (it follows a symlinked
    // profile dir). A tree of the user's own, the global store and an
    // unreadable store or `profiles/` keep their records counting.
    let jobs = crate::claude_jobs::job_trees();
    let clauth_trees: std::collections::HashSet<std::path::PathBuf> =
        crate::runtime::runtime_trees()
            .map(|found| {
                found
                    .trees
                    .iter()
                    .filter_map(|tree| crate::canonical::identity(tree).ok())
                    .collect()
            })
            .unwrap_or_default();
    let clauth_job = |session_id: &str| match jobs.session_tree(session_id) {
        crate::claude_jobs::SessionTree::Named(tree) => clauth_trees.contains(&tree),
        crate::claude_jobs::SessionTree::Global
        | crate::claude_jobs::SessionTree::Untracked
        | crate::claude_jobs::SessionTree::Unknown => false,
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid: u32 = entry
                .file_name()
                .to_str()?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            Some(pid)
        })
        .filter(|pid| !listed(*pid))
        .filter(|pid| {
            crate::session_record::session_record(&dir, *pid).is_some_and(|record| {
                crate::session_record::process_matches_record(*pid, &record)
                    && !clauth_job(&record.session_id)
            })
        })
        .count()
}

#[cfg(test)]
fn switch_list_lines(now: SystemTime) -> Vec<String> {
    let rows = crate::live_sessions::live_rows().expect("the registry lists");
    switch_list_lines_of(&switch_list_rows(rows), now)
}

#[cfg(test)]
fn switch_list_json() -> serde_json::Value {
    let rows = crate::live_sessions::live_rows().expect("the registry lists");
    switch_list_json_of(&switch_list_rows(rows))
}

/// The human table, one line per switchable session, newest first. Columns:
/// sid, account (`start → current` once moved), pending move, the record's
/// name, cwd, age, and the `delegate` tag; `-` fills an absent cell.
fn switch_list_lines_of(
    rows: &[crate::live_sessions::LiveSession],
    now: SystemTime,
) -> Vec<String> {
    let cells: Vec<SwitchListCells> = rows.iter().map(|row| switch_list_cells(row, now)).collect();
    let widths = SwitchListWidths::fit(&cells);
    cells
        .into_iter()
        .map(|c| switch_list_row_line(&c, widths))
        .collect()
}

/// The `--json` array over `rows`: the `--live --json` shape plus the pending
/// move, the record's name and its status; absent cells `null`.
fn switch_list_json_of(rows: &[crate::live_sessions::LiveSession]) -> serde_json::Value {
    serde_json::Value::Array(rows.iter().map(switch_list_json_row).collect())
}

fn switch_list_json_row(row: &crate::live_sessions::LiveSession) -> serde_json::Value {
    let record = live_record_cells(row);
    let mut json = live_json_row_with(row, &record);
    if let Some(object) = json.as_object_mut() {
        object.insert("pending_profile".into(), pending_member(row).into());
        object.insert("name".into(), record.name.into());
        object.insert("status".into(), record.status.into());
    }
    json
}

/// A row's rendered cells, in column order.
struct SwitchListCells {
    sid: String,
    account: String,
    pending: String,
    name: Option<String>,
    cwd: Option<String>,
    age: String,
    tag: String,
}

fn switch_list_cells(row: &crate::live_sessions::LiveSession, now: SystemTime) -> SwitchListCells {
    let record = live_record_cells(row);
    let account = account_cell(row);
    SwitchListCells {
        sid: row.session_id.clone(),
        account,
        pending: pending_member(row).map_or_else(|| "-".to_string(), |m| format!("→ {m}")),
        name: table_cell(record.name),
        cwd: table_cell(record.cwd),
        age: live_age(row.started_at, now),
        tag: if row.is_delegate {
            "delegate".to_string()
        } else {
            String::new()
        },
    }
}

/// The padded widths, measured over every row so one wide cell widens its
/// column for all rows.
#[derive(Debug, Clone, Copy)]
struct SwitchListWidths {
    sid: usize,
    account: usize,
    pending: usize,
    name: usize,
    cwd: usize,
    age: usize,
    tag: usize,
}

impl SwitchListWidths {
    fn fit(cells: &[SwitchListCells]) -> Self {
        let width = grapheme_cells;
        let mut w = Self {
            sid: 0,
            account: 0,
            pending: 0,
            name: 0,
            cwd: 0,
            age: 0,
            tag: 0,
        };
        for c in cells {
            w.sid = w.sid.max(width(&c.sid));
            w.account = w.account.max(width(&c.account));
            w.pending = w.pending.max(width(&c.pending));
            w.name = w.name.max(width(c.name.as_deref().unwrap_or("-")));
            w.cwd = w.cwd.max(width(c.cwd.as_deref().unwrap_or("-")));
            w.age = w.age.max(width(&c.age));
            w.tag = w.tag.max(width(&c.tag));
        }
        w
    }
}

/// One line, columns joined by two spaces and left-padded to their width,
/// trailing blanks trimmed. `-` fills a missing name or cwd.
fn switch_list_row_line(c: &SwitchListCells, w: SwitchListWidths) -> String {
    let name = c.name.as_deref().unwrap_or("-");
    let cwd = c.cwd.as_deref().unwrap_or("-");
    [
        pad_cells(&c.sid, w.sid, false),
        pad_cells(&c.account, w.account, false),
        pad_cells(&c.pending, w.pending, false),
        pad_cells(name, w.name, false),
        pad_cells(cwd, w.cwd, false),
        pad_cells(&c.age, w.age, false),
        pad_cells(&c.tag, w.tag, false),
    ]
    .join("  ")
    .trim_end()
    .to_string()
}

/// The member a session's recorded intent points at while it differs from the
/// account the session runs, else `None`. Shared by the table's pending cell
/// and the `--json` field so the two cannot disagree.
fn pending_member(row: &crate::live_sessions::LiveSession) -> Option<&str> {
    let current = row.current_member.as_deref().unwrap_or(&row.start_profile);
    row.intended_member.as_deref().filter(|i| *i != current)
}

// ── clauth sessions --live ──

/// One sort for every `--live` shape: newest first, equal `started_at` broken
/// by `session_id` in byte order, so the listing never depends on the
/// registry's read order.
fn live_rows_newest_first(
    mut rows: Vec<crate::live_sessions::LiveSession>,
) -> Vec<crate::live_sessions::LiveSession> {
    rows.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    rows
}

/// The live roster's human table, one line per running session, newest first.
/// Columns: sid, account (`start → current` once moved), harness, age,
/// transcript id, cwd, and the tag naming the session's kind (`switch` accepts
/// delegate rows, refuses codex rows by harness).
fn live_lines_of(rows: &[crate::live_sessions::LiveSession], now: SystemTime) -> Vec<String> {
    let cells: Vec<LiveCells> = rows.iter().map(|row| live_cells(row, now)).collect();
    let widths = LiveWidths::fit(&cells);
    cells
        .into_iter()
        .map(|c| live_row_line(&c, widths))
        .collect()
}

/// The `--live --json` array over `rows`, absent cells `null`, `started_at`
/// ISO-8601 UTC.
fn live_json_of(rows: &[crate::live_sessions::LiveSession]) -> serde_json::Value {
    serde_json::Value::Array(rows.iter().map(live_json_row).collect())
}

#[cfg(test)]
fn live_lines(now: SystemTime) -> Vec<String> {
    let rows = crate::live_sessions::live_rows().expect("the registry lists");
    live_lines_of(&live_rows_newest_first(rows), now)
}

#[cfg(test)]
fn live_json() -> serde_json::Value {
    let rows = crate::live_sessions::live_rows().expect("the registry lists");
    live_json_of(&live_rows_newest_first(rows))
}

fn live_json_row(row: &crate::live_sessions::LiveSession) -> serde_json::Value {
    live_json_row_with(row, &live_record_cells(row))
}

/// [`live_json_row`] over record cells the caller already read, so a listing
/// that extends the shape reads the record once.
fn live_json_row_with(
    row: &crate::live_sessions::LiveSession,
    record: &RecordCells,
) -> serde_json::Value {
    serde_json::json!({
        "sid": row.session_id,
        "start_profile": row.start_profile,
        "current_profile": row.current_member.as_deref().unwrap_or(&row.start_profile),
        "harness": row.harness,
        "cwd": record.cwd,
        "started_at": crate::usage::ms_to_iso(row.started_at),
        "transcript_id": record.transcript,
        "delegate": row.is_delegate,
        "background": row.background,
    })
}

/// The cells one row's Claude Code record supplies, read once per row: the
/// transcript id it names, the dir [`crate::session_record::live_cwd`]
/// picks, and the record's name and status (`None` when the record reads none).
fn live_record_cells(row: &crate::live_sessions::LiveSession) -> RecordCells {
    let record = crate::session_record::live_record(row);
    let cwd = crate::session_record::live_cwd(row, record.as_ref());
    RecordCells {
        transcript: record.as_ref().map(|r| r.session_id.clone()),
        name: record
            .as_ref()
            .map(|r| r.name.as_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        status: record
            .as_ref()
            .map(|r| r.status.as_str())
            .filter(|status| !status.is_empty())
            .map(str::to_owned),
        cwd,
    }
}

/// A record-supplied cell as the table prints it: control characters dropped,
/// an empty result read as absent.
fn table_cell(cell: Option<String>) -> Option<String> {
    cell.map(|c| crate::format::terminal_safe(&c))
        .filter(|c| !c.is_empty())
}

/// The account cell: `start → current` only once the session moved to a
/// different member, else the start profile alone. Shared by `--live` and
/// `switch --list` so their account labels cannot drift.
fn account_cell(row: &crate::live_sessions::LiveSession) -> String {
    match row.current_member.as_deref() {
        Some(current) if current != row.start_profile => {
            format!("{} → {}", row.start_profile, current)
        }
        _ => row.start_profile.clone(),
    }
}

/// One row's rendered cells, before the width pass. The account cell comes
/// from [`account_cell`]. The record's own cells drop control and bidi
/// characters, since another program writes that record; `--json` carries
/// them as JSON text for its consumer.
fn live_cells(row: &crate::live_sessions::LiveSession, now: SystemTime) -> LiveCells {
    let record = live_record_cells(row);
    let account = account_cell(row);
    LiveCells {
        sid: row.session_id.clone(),
        account,
        harness: row.harness.as_str().to_string(),
        age: live_age(row.started_at, now),
        transcript: table_cell(record.transcript),
        cwd: table_cell(record.cwd),
        tag: if row.is_delegate {
            "delegate".to_string()
        } else if row.background {
            "background".to_string()
        } else {
            String::new()
        },
    }
}

/// The age cell: the shared one-unit age from `started_at` (epoch ms), the
/// same formatter the full listing's `updated` cell uses.
fn live_age(started_at: u64, now: SystemTime) -> String {
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let start_secs = (started_at / 1000) as i64;
    crate::format::spoken_age((now_secs - start_secs).max(0) as u64, start_secs)
        .unwrap_or_else(|| "-".to_string())
}

/// The cells a row's Claude Code record supplies, before the columns are fitted.
struct RecordCells {
    transcript: Option<String>,
    name: Option<String>,
    status: Option<String>,
    cwd: Option<String>,
}

/// A row's cells, in column order.
struct LiveCells {
    sid: String,
    account: String,
    harness: String,
    age: String,
    transcript: Option<String>,
    cwd: Option<String>,
    tag: String,
}

/// The padded widths, measured over every row so one wide cell widens its
/// column for all rows.
#[derive(Debug, Clone, Copy)]
struct LiveWidths {
    sid: usize,
    account: usize,
    harness: usize,
    age: usize,
    transcript: usize,
    cwd: usize,
    tag: usize,
}

impl LiveWidths {
    fn fit(cells: &[LiveCells]) -> Self {
        let width = grapheme_cells;
        let mut w = Self {
            sid: 0,
            account: 0,
            harness: 0,
            age: 0,
            transcript: 0,
            cwd: 0,
            tag: 0,
        };
        for c in cells {
            w.sid = w.sid.max(width(&c.sid));
            w.account = w.account.max(width(&c.account));
            w.harness = w.harness.max(width(&c.harness));
            w.age = w.age.max(width(&c.age));
            w.transcript = w
                .transcript
                .max(width(c.transcript.as_deref().unwrap_or("-")));
            w.cwd = w.cwd.max(width(c.cwd.as_deref().unwrap_or("-")));
            w.tag = w.tag.max(width(&c.tag));
        }
        w
    }
}

/// One line, columns joined by two spaces and left-padded to their width,
/// trailing blanks trimmed. `-` fills a missing transcript id or cwd.
fn live_row_line(c: &LiveCells, w: LiveWidths) -> String {
    let transcript = c.transcript.as_deref().unwrap_or("-");
    let cwd = c.cwd.as_deref().unwrap_or("-");
    [
        pad_cells(&c.sid, w.sid, false),
        pad_cells(&c.account, w.account, false),
        pad_cells(&c.harness, w.harness, false),
        pad_cells(&c.age, w.age, false),
        pad_cells(transcript, w.transcript, false),
        pad_cells(cwd, w.cwd, false),
        pad_cells(&c.tag, w.tag, false),
    ]
    .join("  ")
    .trim_end()
    .to_string()
}

/// Human table: a workspace header per group, then one row per session. The
/// index already redacted the previews, so nothing is masked here. The token and
/// cost columns appear only under `--tokens`; two permanently blank columns
/// would otherwise eat the width the previews want.
fn emit_sessions_table(groups: &[WorkspaceGroup], tokens: bool) {
    // One clock read for the whole table: every row's age is relative to the
    // same instant, and `session_row` stays pure (its `now` is a parameter,
    // never a read hidden inside).
    for line in table_lines(groups, tokens, SystemTime::now()) {
        outln!("{line}");
    }
}

fn table_lines(groups: &[WorkspaceGroup], tokens: bool, now: SystemTime) -> Vec<String> {
    let cols = Columns::fit(groups.iter().flat_map(|g| &g.sessions), now);
    let mut lines = Vec::new();
    for group in groups {
        let ws = if group.workspace.is_empty() {
            "(unknown workspace)"
        } else {
            &group.workspace
        };
        lines.push(ws.to_string());
        lines.extend(
            group
                .sessions
                .iter()
                .map(|s| session_row(s, tokens, now, cols)),
        );
    }
    lines
}

/// The padded columns' widths, measured over every row of the table, across
/// workspace groups, so one wide cell widens its column for all rows rather
/// than shifting its own row out of line. An id wider than
/// [`Columns::ID_CAP`] is the one exception.
#[derive(Debug, Clone, Copy)]
struct Columns {
    id: usize,
    profile: usize,
    age: usize,
    tokens: usize,
    cost: usize,
}

impl Columns {
    /// The narrowest each column gets: a short uuid prefix, a typical profile
    /// name, and room for a ten-digit token total and a four-digit cost.
    const FLOOR: Self = Self {
        id: 8,
        profile: 12,
        age: 0,
        tokens: 10,
        cost: 8,
    };

    /// The widest the id column grows: a subagent transcript's `agent-<17 hex>`
    /// stem. A named subagent's longer stem shows whole and pushes only its
    /// own row, rather than taking preview width from every row in the table.
    const ID_CAP: usize = 23;

    fn fit<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>, now: SystemTime) -> Self {
        let width = grapheme_cells;
        sessions.into_iter().fold(Self::FLOOR, |cols, s| Self {
            id: cols.id.max(width(short_id(&s.id)).min(Self::ID_CAP)),
            profile: cols.profile.max(width(profile_cell(s))),
            age: cols.age.max(width(&age_half(s, now))),
            tokens: cols.tokens.max(width(&tokens_cell(s))),
            cost: cols.cost.max(width(&cost_cell(s))),
        })
    }
}

/// One session's table row. Pure, so which columns a flag puts in it is
/// assertable without capturing stdout: `now` arrives from the caller, never
/// a clock read hidden inside. The `updated` cell is the 2026-08-22
/// prose-stamp ruling's shape — LOCAL wall clock (`YYYY-MM-DD HH:MM:SS`)
/// paired with its one-unit age, `2026-08-28 14:03:11 (3h ago)`; at 30 days
/// and past the age would repeat the stamp's own date, so the cell prints the
/// stamp alone. The machine ISO shape lives on in the `--json` row only. The
/// token total and its cost are blank when the annotation found none — never
/// `0`, which would read as a real figure — and absent entirely when `tokens`
/// never asked for them.
fn session_row(s: &SessionInfo, tokens: bool, now: SystemTime, cols: Columns) -> String {
    let usage = if tokens {
        format!(
            "  {}  {}",
            pad_cells(&tokens_cell(s), cols.tokens, true),
            pad_cells(&cost_cell(s), cols.cost, true),
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
    // Stamp alone once the age would be a date (the age half is empty); the
    // age column and its surrounding parentheses stay reserved so the
    // token/cost columns align, but only while some row carries an age half;
    // an all-stamp-alone listing keeps the normal column gap instead of a
    // blank age column.
    let age = age_half(s, now);
    let stamp_age = if age.is_empty() {
        let reserve = if cols.age > 0 { 3 + cols.age } else { 0 };
        format!("{updated}{}", " ".repeat(reserve))
    } else {
        let age_cell = format!("({age})");
        format!("{updated} {}", pad_cells(&age_cell, cols.age + 2, false))
    };
    let mut row = format!(
        "  {}  {}  {stamp_age}{usage}  {}",
        pad_cells(short_id(&s.id), cols.id, false),
        pad_cells(profile_cell(s), cols.profile, false),
        preview_pair(s),
    );
    // An empty preview would leave the padding as trailing blanks.
    row.truncate(row.trim_end().len());
    row
}

fn profile_cell(s: &SessionInfo) -> &str {
    s.last_ran_profile.as_deref().unwrap_or("-")
}

/// The age half of the `updated (age)` cell: the shared one-unit age, empty
/// once the age reaches the 30-day date arm — the full local stamp already
/// carries the date, so the cell renders the stamp alone rather than a
/// duplicate date. The shared formatter spells a non-positive span `just now`
/// itself, so no ` ago` pairing is stitched on here: a sub-second-fresh file or
/// a future mtime reads `just now`, never `now ago`.
fn age_half(s: &SessionInfo, now: SystemTime) -> String {
    let age_secs = now
        .duration_since(s.updated)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if crate::format::age_takes_a_date(age_secs.max(0) as u64) {
        return String::new();
    }
    let epoch = s
        .updated
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::format::spoken_age(age_secs.max(0) as u64, epoch).unwrap_or_else(|| "-".to_string())
}

fn tokens_cell(s: &SessionInfo) -> String {
    s.tokens.map(|t| t.to_string()).unwrap_or_default()
}

fn cost_cell(s: &SessionInfo) -> String {
    s.cost.map(|c| format!("${c:.2}")).unwrap_or_default()
}

/// The first block of a uuid session id, enough to eyeball in the table (the
/// full id is what `clauth resume`/`info` take). A non-uuid stem shows whole.
fn short_id(id: &str) -> &str {
    if id.len() == 36
        && id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
        && id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit())
    {
        &id[..8]
    } else {
        id
    }
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

#[cfg(test)]
#[path = "../tests/inline/sessions_cli.rs"]
mod tests;
