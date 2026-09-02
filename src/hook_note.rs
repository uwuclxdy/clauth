//! `clauth hook-profile-changed-note` — tell a running conversation when the
//! account behind it changed.
//!
//! A conversation can move accounts three ways and none of them says so: a
//! resume under another profile keeps the Claude Code session id and appends to
//! the same transcript, a `clauth switch` lands while a global session works,
//! and a `--with-fallback` session executes a credential swap mid-run. One
//! predicate answers all three — which account this conversation's credentials
//! resolve to right now, against the last value clauth told this conversation —
//! so this reads a hook payload on stdin and emits `additionalContext` when those
//! two differ. A second emit, the headroom nudge, rides the same subcommand: a
//! parent-scope `Task` (agent-spawn) fire whose account's 5h window is
//! exhausted, or burning toward the cap, with nothing left to catch the
//! failure earns it (r8 gate; r7 shipped the static form). r9: the switched
//! spelling appends the new account's live 5h window percent — the same
//! disk-cache read the nudge uses — so the reader no longer has to call
//! `profiles` for the deciding figure; a usage-less account keeps the
//! sentence unchanged.
//!
//! **Not the MCP `instructions` block.** That block is built once per process,
//! so it cannot carry a mid-conversation change at all, and rewriting the front
//! of a live context invalidates the cached prefix behind it.
//!
//! Three properties carry the design:
//!
//! - **The account comes from the tier walk, never the runtime directory name.**
//!   After a swap the directory keeps the profile the session LAUNCHED on while
//!   the credential link points elsewhere, so a path-derived name answers "which
//!   directory" rather than "which account".
//! - **A stat gates the resolution, and a TTL bounds what the stat misses.**
//!   This runs on every tool call, so the record carries a stamp of the
//!   resolution's inputs and skips it when nothing moved. The stamp is the
//!   credential store plus a hash of [`crate::profile::reload_fingerprint`],
//!   which is the crate's own predicate for "could a config reload change the
//!   answer" and covers every per-profile `config.toml` that two hand-rolled
//!   stats did not. [`RESOLUTION_TTL`] is the correctness backstop rather than
//!   an optimisation: it turns anything the fingerprint still misses from an
//!   unbounded miss into a bounded one. Two costs, and the doc used to price
//!   only the first (both measured by review, 2026-08-21, debug build):
//!   a fire that OPENS the gate runs `load_config`, which chmod-walks the whole
//!   `~/.clauth` tree, so this process mutates the filesystem when it resolves
//!   (~3.1 ms at 0 entries, ~5.9 ms at 2000, against a ~2.2 ms spawn floor);
//!   and `reload_fingerprint` runs on EVERY fire, open or closed, at a readdir
//!   plus two stats per profile (+342 µs at 2 profiles, +523 at 30, +651 at 60).
//!   The second is the price of the gate being sound at all, and it scales with
//!   profile count rather than with anything this module controls.
//! - **One record per SCOPE, not per conversation.** A `PostToolUse` fired
//!   inside a subagent carries `agent_id`; a single shared told-flag would let
//!   the first subagent to fire consume the note while the main thread never
//!   hears it. Separate files keep two SCOPES off each other's bytes, and that
//!   is all they do: every main-thread parallel tool call fires with no
//!   `agent_id` and so shares one record, which is why the read-modify-write
//!   below is flock-held (measured by review: 4 concurrent fires emitted the
//!   note 2-4 times in 30 of 30 trials before the lock).
//!
//! - **The nudge says what the chain cannot save.** A `Task` fire spends the
//!   same 5h pool an agent spawn is refused against, and nothing in the turn's
//!   context names the window before the 429 lands. The nudge answers "would a
//!   switch catch this": the burn-projection gate r8 ships, the fallback-chain
//!   walk replayed over the disk cache, and the live-session registry's
//!   `follows_chain` — a session the chain may move already hears about the
//!   move one tool call after it lands. Suppression and re-arm ride the same
//!   record the account note uses.
//!
//! A failure is silence at exit 0 wherever this module can make it one, because
//! a hook that errors on a tool call breaks the conversation it exists to
//! inform. The one path it does not own is [`crate::out`]: a stdout write error
//! that is not `BrokenPipe` panics there by that module's deliberate contract,
//! which exits 101.

use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::out::outln;
use crate::profile::atomic_write_600;

/// Dir under `~/.clauth` holding one record per conversation scope.
const RECORDS_DIR: &str = "conversations";

/// How long a record whose transcript is not on disk survives the sweep.
///
/// The grace belongs on THIS branch, not on the ageing one below: a baseline
/// recorded at `SessionStart` can land before Claude Code has created the
/// transcript file, and a bare `!exists()` then lets any `clauth` invocation on
/// the box reap a live conversation's record — after which its next real account
/// move is absorbed as a fresh baseline and never announced.
///
/// It is measured from the record's MTIME, and every fire moves that mtime
/// ([`touch_record`]) — except one whose record write failed, which logs its
/// own suppression — so the quantity it bounds is time since this scope last
/// FIRED, not time since the transcript went missing. A conversation still
/// firing never elapses it, whatever its transcript's state, which is the
/// intent; the two only coincide for a scope that has gone quiet.
const MISSING_TRANSCRIPT_GRACE: Duration = Duration::from_secs(60 * 60);

/// How long a record that never carried a `transcript_path` survives. There is
/// no transcript to test, so the fire-mtime ([`touch_record`]'s) is the only
/// liveness signal such a record has — this is how long it may sit silent
/// before it is reaped.
const ORPHAN_RECORD_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a resolution may be reused before it is retaken regardless of the
/// stamp. The stamp is an optimisation and this is the correctness bound: any
/// input `reload_fingerprint` does not cover (a per-profile `credentials.json`
/// write that never touches the live link) costs at most this much staleness
/// rather than an unbounded miss.
const RESOLUTION_TTL: Duration = Duration::from_secs(60);

/// Longest stdin this will read. `PostToolUse` embeds a whole `tool_response`,
/// and the hook manifest's `timeout` bounds TIME rather than memory: reading an
/// unbounded stream reached 28.4 GB RSS in review. Matches the 10 MB cap
/// `update.rs` already puts on a downloaded asset.
const MAX_PAYLOAD_BYTES: u64 = 10 * 1024 * 1024;

/// The two spellings, behind one renderer so they cannot drift apart. The
/// switched spelling carries an optional headroom clause (r9, reviewzy entry
/// E4, 2026-08-28): the new account's live 5h window percent when the disk
/// cache holds one, omitted — the sentence byte-identical to the pre-r9
/// spelling — when it does not.
///
/// The noun is "session", by owner ruling on 2026-08-21, superseding an earlier
/// one here that said "conversation" and never "session". Carry the cost that
/// ruling turned on rather than deleting it: every other "session" in
/// model-facing clauth copy names the PROCESS, so after a swap the MCP block's
/// runtime-paths note and this note both say "session" about two things that
/// disagree. Do not resolve that by mutating the block, which is settled against.
enum Note<'a> {
    /// A new Claude Code process picked this conversation up on another account.
    Resumed { now: &'a str, before: &'a str },
    /// The account moved under a conversation that was already running.
    Switched {
        from: &'a str,
        to: &'a str,
        /// The new account's live 5h window percent, when a figure exists.
        used: Option<f64>,
    },
}

impl Note<'_> {
    fn render(&self) -> String {
        match *self {
            Note::Resumed { now, before } => format!(
                "clauth note: session resumed under `{now}`; earlier turns ran under `{before}`."
            ),
            Note::Switched {
                from,
                to,
                used: Some(used),
            } => format!(
                "clauth note: the active profile for this session switched from `{from}` to `{to}`; its 5h window is {pct}% used.",
                pct = crate::format::format_pct(used).trim_end_matches('%'),
            ),
            Note::Switched {
                from,
                to,
                used: None,
            } => format!(
                "clauth note: the active profile for this session switched from `{from}` to `{to}`."
            ),
        }
    }
}

/// The fields of a hook payload this subcommand reads; everything else is
/// ignored.
struct Payload {
    /// Echoed back in the output envelope, so the host routes the context to the
    /// event it came from.
    event: String,
    session_id: String,
    /// Present only on a fire from inside a subagent, which is what makes it the
    /// per-call scope key.
    agent_id: Option<String>,
    /// `PostToolUse` only: the tool that fired. `Task` is Claude Code's
    /// agent-spawn tool, the one call the headroom nudge gates on.
    tool_name: Option<String>,
    /// `SessionStart` only. Claude Code documents five: `startup`, `resume`,
    /// `clear`, `compact`, `fork`. Anything this does not recognise rebaselines
    /// silently, because every source Claude Code has added so far marks a
    /// context boundary, and announcing a switch about turns a fresh context
    /// never held is the worse failure.
    source: Option<String>,
    /// Recorded so the sweep can reap a record whose conversation is gone.
    transcript: Option<PathBuf>,
}

/// One scope's memory of what it was last told, plus the cache that lets the
/// common fire answer without resolving anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct NoteRecord {
    /// The account this scope was last told about. `None` until a first fire
    /// establishes the baseline — there are no earlier turns to correct then.
    #[serde(default)]
    told: Option<String>,
    /// The note last emitted to this scope. Compaction drops injected context
    /// while `told` would suppress a second note, so the stored text is what a
    /// `source: "compact"` fire re-announces.
    #[serde(default)]
    last_note: Option<String>,
    /// Stamp of the resolution's inputs when `resolved` was taken.
    ///
    /// Written ONLY when the resolution attributed an account. Caching a `None`
    /// here would bank the very stamp move that opened the gate, and nothing
    /// moves it again — so the note would be lost rather than deferred, for the
    /// life of the conversation. An ordinary rotation reaches that: it writes
    /// the live file (stamped) and then the profile store (not).
    #[serde(default)]
    watch: Option<Watch>,
    /// What the last resolution answered, cached behind `watch` + [`resolved_at`].
    /// Never `Some(None)` in effect: an unattributed read is not cached at all.
    #[serde(default)]
    resolved: Option<String>,
    /// The instant the credential read behind `resolved` was taken, for
    /// [`RESOLUTION_TTL`].
    #[serde(default)]
    resolved_at: Option<SystemTime>,
    /// This conversation's transcript, for the sweep.
    #[serde(default)]
    transcript: Option<PathBuf>,
    /// The headroom nudge's last-emitted state (r7). `None` on every record
    /// written before the field existed — the `#[serde(default)]` upgrade gate
    /// that keeps old records parsing.
    #[serde(default)]
    nudge: Option<NudgeState>,
}

/// The headroom nudge's memory for this scope: which 5h window the last verdict
/// was taken against, and whether the note was emitted for it. A verdict is
/// "unchanged" per WINDOW, never forever: a different `resets_at` (the window
/// rolled over) re-arms, and a silent verdict on a window that was told flips
/// `emitted` back so a later true verdict in the same window re-announces —
/// the two re-arms r8's projection gate inherits unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NudgeState {
    /// The window's reset instant, epoch seconds — the identity comparison.
    #[serde(default)]
    resets_at: Option<i64>,
    #[serde(default)]
    emitted: bool,
}

impl NoteRecord {
    /// Whether the record already holds an observation at least as new as
    /// `taken_at` — and one taken in the PAST.
    ///
    /// Both halves earn their place. Without the first, a fire that resolved
    /// before a peer overwrites the fresher verdict and announces the reversal.
    /// Without the second, one backward clock step (chrony/timesyncd stepping a
    /// large offset, a VM snapshot restore, a suspend/resume — all of which land
    /// exactly when sessions start) leaves `resolved_at` in the future and every
    /// later fire defers to it, discarding correct answers for the size of the
    /// step. [`RESOLUTION_TTL`] cannot bound that: this runs on the path
    /// [`cache_holds`] has already rejected, which is why the fire resolved.
    ///
    /// `>=` rather than `>`: on a tie both fires would otherwise cache and the
    /// later ARRIVER would win regardless of who observed first (measured at
    /// ~0.0095% of simultaneous stamp pairs). A fire stamps exactly once, so it
    /// can never tie with its own prior write.
    fn holds_a_newer_observation_than(&self, taken_at: SystemTime) -> bool {
        self.resolved_at
            .is_some_and(|held| held >= taken_at && held <= SystemTime::now())
    }

    /// Whether the cached resolution still answers for `watch`: an account was
    /// attributed, the stamped inputs have not moved, and the answer is younger
    /// than [`RESOLUTION_TTL`]. All three, because the stamp alone has been
    /// measured to miss an input and a miss it cannot see is unbounded.
    fn cache_holds(&self, watch: &Watch) -> bool {
        self.resolved.is_some()
            && self.watch.as_ref() == Some(watch)
            && self.resolved_at.is_some_and(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age < RESOLUTION_TTL)
            })
    }
}

/// The inputs the attributed account is taken from, as far as a stat can see
/// them. Deliberately not a complete account of `load_config`'s reads — see
/// [`RESOLUTION_TTL`] for what bounds the remainder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Watch {
    /// The credential store this conversation's Claude Code loads. Followed
    /// through the link, since a swap repoints it and stamps the TARGET.
    creds: Option<Stamp>,
    /// Hash of [`crate::profile::reload_fingerprint`], the crate's own predicate
    /// for "could a config reload change the answer". It covers `profiles.toml`
    /// AND every per-profile `config.toml` and `session-token.json` — the ones a
    /// hand-rolled pair of stats missed, which let a `disabled = true` flip
    /// change the attributed account behind a closed gate.
    ///
    /// Hashed rather than stored whole because the record is JSON and the
    /// fingerprint is not a serde type. A hasher change across releases shifts
    /// every stored value at once, which opens the gate one extra time per
    /// conversation and costs one resolution.
    config: u64,
}

/// Mtime and length of one watched file, `None` when it is absent. Length rides
/// along because an mtime alone cannot separate two writes a coarse filesystem
/// truncates into one tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    mtime: SystemTime,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        mtime: meta.modified().ok()?,
        len: meta.len(),
    })
}

/// Stamp both inputs. Fails soft: an unresolvable credential path contributes
/// `None`, which compares equal to itself and so gates exactly like an absent
/// file. `reload_fingerprint` fails soft on its own terms (a stat error
/// contributes the empty value).
fn watch_now() -> Watch {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    crate::profile::reload_fingerprint().hash(&mut hasher);
    Watch {
        creds: crate::which::active_credentials_path()
            .as_deref()
            .and_then(stamp),
        config: hasher.finish(),
    }
}

/// One account reading plus the instant its credential read was taken — the
/// pair [`note_for`]'s staleness guard compares.
struct Reading {
    account: Option<String>,
    taken_at: SystemTime,
}

/// The account a loaded config's credentials resolve to, through the same tier
/// walk `clauth which` uses, stamped with the instant of the credential read.
/// The stamp is taken IMMEDIATELY before the resolve — the credential read is
/// the first thing `resolve_active` does.
///
/// The stamp is the observation order two racing fires compare in
/// [`note_for`], and it is taken here, at the read, rather than at the fire's
/// start, because "when the resolve started" is only a PROXY for "when it
/// looked": two fires starting together can read opposite sides of a switch
/// landing inside their resolve windows, and with a start-of-resolve stamp the
/// staler reading can carry the later stamp, pass the guard, and announce the
/// reversal. Anchoring the stamp at the read makes a staler reading carry the
/// earlier stamp by construction.
///
/// NOT a total guarantee. A switch landing inside the remaining window —
/// between the stamp and the credential read, or between that read and a later
/// tier's reads (the CLA-SPLIT sidecars) — still inverts, and the config half
/// of the answer was read earlier, by `load_config`. The window is the read's
/// own duration, against the whole resolve start the stamp used to be taken
/// at, and an inversion still self-corrects at [`RESOLUTION_TTL`].
fn resolve_account(config: &crate::profile::AppConfig) -> Reading {
    let taken_at = SystemTime::now();
    let account = crate::which::resolve_active(config).map(|(name, _)| name);
    Reading { account, taken_at }
}

pub(crate) fn run() -> Result<()> {
    let mut input = String::new();
    // Bounded, not because a hostile payload is expected but because the host
    // supplies it and an unbounded read has no ceiling but RAM. A truncated
    // payload fails to parse and the fire goes silent, which is the same
    // outcome as any other malformed input.
    let _ = std::io::stdin()
        .take(MAX_PAYLOAD_BYTES)
        .read_to_string(&mut input);
    let Some(payload) = parse_payload(&input) else {
        return Ok(());
    };
    // One `load_config` per fire, shared by its two consumers — the account
    // note's resolve (gate-open only) and the nudge reader (every eligible
    // `Task` fire). A gate-open `Task` fire paid it twice before, ~3.1-5.9 ms
    // per load (the chmod-walk, priced in the module doc). Lazy, so a fire
    // that opens neither consumer never loads; a failed load is retried by
    // the reader exactly as it was before, off the same bytes microseconds
    // later.
    let config: std::cell::OnceCell<Option<crate::profile::AppConfig>> = std::cell::OnceCell::new();
    let resolve = || {
        config
            .get_or_init(|| crate::profile::load_config().ok())
            .as_ref()
            .map(resolve_account)
    };
    let mut notes: Vec<String> = Vec::new();
    if let Some(note) = note_for(&payload, &watch_now(), &resolve) {
        notes.push(note);
    }
    if let Some(read) = read_nudge(&payload, config.get().and_then(|loaded| loaded.as_ref()))
        && let Some(note) = nudge_note(&payload, &read)
    {
        notes.push(note);
    }
    // One envelope, whatever fired: two JSON documents on stdout would parse
    // as none, and one `additionalContext` field carries both notes when both
    // earned the turn.
    if !notes.is_empty() {
        outln!("{}", joined_envelope(&payload.event, &notes));
    }
    Ok(())
}

/// [`envelope`] over the fire's earned notes joined — whatever earned the turn,
/// one note or two, renders as ONE `additionalContext` field and so ONE JSON
/// document on stdout (two documents would parse as none). Split from the
/// print for the same reason [`envelope`] is: the join is assertable without
/// capturing stdout.
fn joined_envelope(event: &str, notes: &[String]) -> serde_json::Value {
    envelope(event, &notes.join("\n\n"))
}

/// The hook's output payload, split from the print so its field shapes are
/// assertable without capturing stdout.
///
/// `hookEventName` is echoed from the payload rather than chosen here, so the
/// host routes the context back to the event that produced it. The note itself
/// reads as fact and never as an instruction: Claude Code's injection defenses
/// surface command-style text to the user instead of feeding it to the model.
fn envelope(event: &str, note: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": note,
        }
    })
}

fn parse_payload(input: &str) -> Option<Payload> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    if !is_bare_id(&session_id) {
        return None;
    }
    // Keyed on the field being ABSENT, never on `as_str()` succeeding. A
    // present-but-unusable value (a number, a bool, an object, or a string that
    // cannot spell a filename) belongs to a subagent whose scope cannot be
    // named, and treating it as absent consumes the main thread's record — the
    // one scope it must never touch. `as_str()` alone read a `12345` as absent.
    let agent_id = match value.get("agent_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(present) => match present.as_str() {
            Some(id) if is_bare_id(id) => Some(id.to_string()),
            _ => return None,
        },
    };
    // The event name is echoed into the envelope, so it is bounded. Unbounded, a
    // 1 MB `hook_event_name` came back as 1 MB on stdout.
    let event = value.get("hook_event_name")?.as_str()?;
    if !is_echoable_event(event) {
        return None;
    }
    // Lenient where `agent_id` is strict: a present-but-unusable tool name is
    // not a filename risk and cannot spell `Task`, so it reads as absent and
    // simply never earns the nudge — refusing the whole payload over it would
    // take the account note down with it.
    let tool_name = value
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some(Payload {
        event: event.to_string(),
        session_id,
        agent_id,
        tool_name,
        source: value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        transcript: value
            .get("transcript_path")
            .and_then(serde_json::Value::as_str)
            // Absolute and non-empty, or it is not a path the SWEEP can test for
            // liveness. `Path::new("").exists()` is false, which reaped live
            // records; a relative one resolves against the sweeping process's
            // cwd (a daemon, a `clauth start`), never the hook's.
            .filter(|p| !p.is_empty() && Path::new(p).is_absolute())
            .map(PathBuf::from),
    })
}

/// Whether `s` can spell a path COMPONENT on its own. Both ids arrive in a
/// payload this process does not author and both reach a filename, so each is
/// checked before any join.
fn is_bare_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `s` is safe to echo back as `hookEventName`.
///
/// Deliberately looser than [`is_bare_id`], and separate from it, because this
/// value never reaches a filename — it only has to be bounded and free of
/// anything that could break the envelope for a reader. Sharing the id charset
/// would take the hook silently offline for any event Claude Code ever
/// namespaces (`a.b`, `a:b`), with the failure looking like the feature simply
/// not firing.
fn is_echoable_event(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.chars().any(char::is_control)
}

fn records_dir() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join(RECORDS_DIR))
}

/// One record per (conversation, scope). The `.` separator is what keeps the two
/// shapes apart: [`is_bare_id`] admits no dot, so a subagent's file can never
/// spell the bare conversation's.
fn record_path(session_id: &str, agent_id: Option<&str>) -> Result<PathBuf> {
    let name = match agent_id {
        Some(agent) => format!("{session_id}.{agent}.json"),
        None => format!("{session_id}.json"),
    };
    Ok(records_dir()?.join(name))
}

fn load_record(path: &Path) -> Option<NoteRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Owner-only like every `~/.clauth` write: a record names the account a
/// conversation runs on and where its transcript sits.
fn store_record(path: &Path, record: &NoteRecord) -> Result<()> {
    atomic_write_600(path, serde_json::to_vec(record)?)?;
    Ok(())
}

/// The account a conversation's main scope was last told about — the durable
/// `told` baseline the hook maintains. This is the read the `delegate` resume
/// inference takes: `resolved` is deliberately not it, being a TTL-bounded
/// cache that only ever holds the last ATTRIBUTED answer, while `told` is the
/// baseline a conversation carries across processes.
///
/// `None` when the id cannot name a record (the hook only ever writes records
/// for bare ids, so a path- or dot-shaped id has none by construction), when
/// no record exists, or when the record never established a baseline. A plain
/// file read under no lock: writers replace the record atomically (temp +
/// rename), so a racing read parses the old or the new bytes whole, and a
/// failed parse answers `None` rather than a wrong account.
pub(crate) fn told_account(session_id: &str) -> Option<String> {
    // The id reaches a filename in `record_path`, so it is checked at this
    // boundary the same way the hook checks the one in its own payload.
    if !is_bare_id(session_id) {
        return None;
    }
    let path = record_path(session_id, None).ok()?;
    load_record(&path)?.told
}

/// The account the hook last resolved for a conversation's main scope — the
/// exact per-conversation observation the session→profile attribution consults
/// in place of the mtime sweep. `resolved` is the last account actually
/// attributed, where `told` is the note-suppression baseline. Same shape as
/// [`told_account`]: a bare id only, `None` when no record exists or the
/// record never attributed an account, which the sweep then covers.
pub(crate) fn resolved_account(session_id: &str) -> Option<String> {
    if !is_bare_id(session_id) {
        return None;
    }
    let path = record_path(session_id, None).ok()?;
    load_record(&path)?.resolved
}

/// Decide what this fire says and store what it learned.
///
/// `resolve` is taken by reference so a test can count how often the gate lets it
/// through and control the reading's stamp; nothing else varies it.
fn note_for(
    payload: &Payload,
    watch: &Watch,
    resolve: &dyn Fn() -> Option<Reading>,
) -> Option<String> {
    let path = record_path(&payload.session_id, payload.agent_id.as_deref()).ok()?;

    // Peek UNLOCKED, only to decide whether the slow half is needed. `resolve`
    // goes through `load_config`, which chmod-walks the whole `~/.clauth` tree,
    // and that must never run inside the hold below.
    let peek = load_record(&path);
    let fresh = match peek.as_ref().filter(|p| p.cache_holds(watch)) {
        Some(_) => None,
        // A fire that opens the gate resolves now; the reading carries its own
        // stamp, taken at the credential read — [`resolve_account`] owns why it
        // lives there and what it does not guarantee.
        _ => resolve(),
    };

    // The account a switch would name as the new one — the peek's cached
    // answer on a closed gate, this fire's own resolve on an open one — and
    // the headroom figure for it, gathered BEFORE the hold: a usage-cache read
    // must not run inside the read-modify-write. The peek is not the verdict
    // (the copy under the lock outranks it), so the figure carries its account
    // and the emit below uses it only when the note's `to` is that account — a
    // figure read for the wrong account is a false claim, and omission is this
    // module's failure direction. Gated on a possible change: in the common
    // path, `told` equal to the candidate means no note fires and the read is
    // wasted; the documented stamp-inversion corner can still announce a false
    // reversal with the gather skipped, clause-less — omission, not a wrong
    // figure.
    let candidate = match &fresh {
        None => peek.as_ref().and_then(|p| p.resolved.as_deref()),
        Some(reading) => reading.account.as_deref(),
    };
    let switched_headroom = candidate
        .filter(|account| peek.as_ref().and_then(|p| p.told.as_deref()) != Some(*account))
        .and_then(|account| {
            switched_headroom_pct(account).map(|used| SwitchedHeadroom {
                account: account.to_string(),
                used,
            })
        });

    let _hold = ScopeLock::acquire();
    // Re-read INSIDE the hold. The peek above may be another writer's stale
    // bytes: a scope is not one writer, since every main-thread parallel tool
    // call fires with no `agent_id` and lands here.
    let stored = load_record(&path);
    let mut record = stored.clone().unwrap_or_else(|| NoteRecord {
        // A scope with no record of its own inherits the conversation's
        // baseline. A fresh `told` would adopt the CURRENT account as this
        // scope's first observation, so a subagent firing after the change would
        // silently eat the note instead of hearing it.
        told: inherited_baseline(payload),
        ..NoteRecord::default()
    });
    if payload.transcript.is_some() {
        record.transcript = payload.transcript.clone();
    }
    let current = match fresh {
        // The cache still answers, and the copy under the lock outranks the peek.
        None => record.resolved.clone(),
        // A fire whose observation PREDATES the one already recorded is carrying
        // the staler reading, whatever order the two reached the lock in:
        // resolving happens outside the hold, so arrival order says nothing
        // about observation order, and the 2 s lock wait widens that gap rather
        // than closing it. Defer to the record, exactly as the cache-hit branch
        // above does. Overwriting instead let a slow fire announce the reversal
        // (`switched from cld to kerry` for a switch that never happened) and
        // cache its stale answer for the whole TTL.
        Some(Reading { taken_at, .. }) if record.holds_a_newer_observation_than(taken_at) => {
            record.resolved.clone()
        }
        Some(Reading { account, taken_at }) => {
            // Only an ATTRIBUTED answer is cached. See `NoteRecord::watch`.
            if account.is_some() {
                record.watch = Some(watch.clone());
                record.resolved = account.clone();
                record.resolved_at = Some(taken_at);
            }
            account
        }
    };
    let used = switched_headroom
        .filter(|h| current.as_deref() == Some(h.account.as_str()))
        .map(|h| h.used);
    let note = decide(payload, &mut record, current.as_deref(), used);
    if stored.as_ref() != Some(&record) {
        if store_record(&path, &record).is_err() {
            // The record IS the suppression mechanism, so a note that cannot be
            // remembered is re-emitted on every tool call for the life of the
            // conversation. Keyed on the write failing at all rather than on any one
            // cause: a full disk and a read-only tree reach this the same way.
            // The log FILE, not `logline!`. This runs once per tool call, so a
            // persistent failure through the routed sink lands on a hook's
            // (non-terminal) stderr once per fire — the same unbounded flood this
            // suppression exists to prevent, moved onto the channel Claude Code
            // shows the user. The file is size-rotated; stderr is not.
            crate::logline::to_logfile(format_args!(
                "hook-note: cannot persist {}; staying silent",
                path.display()
            ));
            return None;
        }
    } else {
        // An unchanged record still means a LIVE fire: the sweep's grace reads
        // this file's mtime, so every fire must move it or a conversation
        // firing past the grace loses its baseline to the reap.
        touch_record(&path);
    }
    note
}

/// Move a record's mtime to now without rewriting its bytes: the sweep's
/// [`MISSING_TRANSCRIPT_GRACE`] measures this mtime, and it must mean "last
/// FIRE". `note_for` rewrites only a record that changed, so an unchanged
/// record's mtime would otherwise age mid-conversation and the sweep would
/// reap a live scope's baseline — the defect this exists to close (measured:
/// 0/40 announced against a reap-eligible record, 40/40 against a fresh one;
/// see the sweep's doc). The unchanged record is the common fire, so the
/// cheap open-and-stamp is the per-fire price the predicate needs. A failure
/// degrades to the pre-touch behavior and is logged, because silent it would
/// leave the sweep reaping live records with no trace of why.
fn touch_record(path: &Path) {
    if std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .is_err()
    {
        crate::logline::to_logfile(format_args!(
            "hook-note: cannot stamp the last fire on {}; the sweep may reap a live record",
            path.display()
        ));
    }
}

/// An exclusive hold over the records dir for one read-modify-write.
///
/// A LEAF in the lock order: nothing is acquired while it is held, and the
/// resolution that would reach `~/.clauth`'s own state lock runs before it. One
/// lock file for the whole dir rather than one per scope, because the hold is a
/// read plus a rename and the only contention is a fan-out's own fires, so
/// per-scope granularity would buy nothing and add a second file to reap.
///
/// Failing to take it degrades to the pre-lock behaviour (a possible duplicate
/// note) rather than to silence: a hook must not block a tool call on a lock.
/// The deadline is also what keeps a NESTED acquisition soft — `flock` blocks a
/// second fd in the same process, so a future caller that takes this around
/// something already holding it degrades after the wait instead of hanging.
/// Today there is no such nesting: `note_for`, `nudge_note` and
/// `gc_conversation_records` are the only holders and none reaches another.
/// `nudge_note` holds across the same shape as `note_for` — a record read,
/// the verdict, an `atomic_write_600` and a log-file append — and its
/// expensive reads (the config load, the cache reads, the chain-walk replay)
/// all run before the acquisition, in `read_nudge`.
struct ScopeLock {
    /// Held open for the guard's lifetime and never read: closing the fd is what
    /// releases the flock, so the binding IS the lock. Named like `StateLock`'s
    /// own guards for the same reason.
    _held: Option<std::fs::File>,
}

impl ScopeLock {
    fn acquire() -> Self {
        const WAIT: Duration = Duration::from_secs(2);
        let held = (|| {
            let dir = records_dir().ok()?;
            crate::profile::mkdir_700(&dir).ok()?;
            let file = crate::profile::open_state_file(&dir.join(".lock")).ok()?;
            if let Err(e) = crate::lock::lock_file_with_timeout(&file, WAIT) {
                // Never swallowed: proceeding unlocked is duplicate notes
                // coming back, and without this the only diagnostic that
                // exists is discarded and the degradation is silent.
                crate::logline::to_logfile(format_args!(
                    "hook-note: proceeding without the scope lock: {e}"
                ));
                return None;
            }
            Some(file)
        })();
        Self { _held: held }
    }
}

/// What a scope firing for the first time treats as its starting account: the
/// main thread's, when this is a subagent. The main thread has no one to inherit
/// from, so its own first fire is the baseline.
fn inherited_baseline(payload: &Payload) -> Option<String> {
    payload.agent_id.as_ref()?;
    let main = record_path(&payload.session_id, None).ok()?;
    load_record(&main)?.told
}

/// The change test, against the record this scope carries. `current` is `None`
/// when clauth cannot attribute the loaded credentials. `used` is the new
/// account's live 5h window percent — `None` renders the switched sentence
/// without the headroom clause, the pre-r9 spelling for a usage-less account.
fn decide(
    payload: &Payload,
    record: &mut NoteRecord,
    current: Option<&str>,
    used: Option<f64>,
) -> Option<String> {
    // An unattributable credential is not evidence that anything moved: a
    // disabled profile, a `claude login` clauth holds no copy of, and a config
    // it could not parse all land here. Leaving `told` standing is what keeps a
    // later real move rendering both real names instead of one and a shrug.
    let current = current?;
    if payload.event == "SessionStart" {
        match payload.source.as_deref() {
            Some("resume") => {
                return match record.told.as_deref() {
                    Some(before) if before != current => {
                        let note = Note::Resumed {
                            now: current,
                            before,
                        }
                        .render();
                        Some(tell(record, current, note))
                    }
                    Some(_) => {
                        // Same account across the restart. Drop whatever the
                        // PREVIOUS process emitted, or a later compaction
                        // re-announces a switch belonging to a process this
                        // context never saw — and re-announces it every time.
                        //
                        // Rests on an UNVERIFIED premise: that hook-injected
                        // `additionalContext` is not replayed into the resumed
                        // context. If Claude Code does replay it, the context
                        // did see that note and re-announcing was right. Nobody
                        // has measured which; the behaviour is pinned either way
                        // by `a_resume_on_the_same_account_drops_the_previous_processes_note`.
                        record.last_note = None;
                        None
                    }
                    None => {
                        record.told = Some(current.to_string());
                        None
                    }
                };
            }
            // Compaction dropped whatever was injected, while the record would
            // suppress a second note — so without this a conversation that
            // compacts after a change is left believing the old account.
            Some("compact") => {
                return match record.told.as_deref() {
                    Some(before) if before != current => {
                        let note = Note::Switched {
                            from: before,
                            to: current,
                            used,
                        }
                        .render();
                        Some(tell(record, current, note))
                    }
                    Some(_) => record.last_note.clone(),
                    None => {
                        // A compaction before anything was ever told. There is
                        // nothing to re-announce, and returning without setting
                        // `told` would leave the scope baseline-less for another
                        // fire.
                        record.told = Some(current.to_string());
                        None
                    }
                };
            }
            // `startup`, `clear`, `fork`, and anything Claude Code adds later.
            // A fresh context holds no earlier turns to correct, and every
            // source added so far marks a context boundary — so an unrecognised
            // one rebaselines rather than announcing a switch about turns that
            // never existed.
            _ => {
                record.told = Some(current.to_string());
                record.last_note = None;
                return None;
            }
        }
    }
    match record.told.as_deref() {
        Some(before) if before != current => {
            let note = Note::Switched {
                from: before,
                to: current,
                used,
            }
            .render();
            Some(tell(record, current, note))
        }
        Some(_) => None,
        None => {
            record.told = Some(current.to_string());
            None
        }
    }
}

/// Record `note` as this scope's newest and hand it back. One place, so `told`
/// and `last_note` cannot advance apart.
fn tell(record: &mut NoteRecord, account: &str, note: String) -> String {
    record.told = Some(account.to_string());
    record.last_note = Some(note.clone());
    note
}

/// The figure the account-changed note appends: which account it was read for,
/// and that account's live 5h window percent.
///
/// One pair, not a bare figure: the read happens on the UNLOCKED peek, whose
/// answer the copy under the scope lock can outrank, so the emit uses the
/// figure only when the note's `to` is the account it was read for. Every
/// `told` write that can reach the store lands the account the fire resolved —
/// a differing creation baseline is rewritten by the emit before it is stored
/// — so a persisted record has `told` and `resolved` equal whenever both are
/// Some: the pairing's drop branch is unreachable today. It exists to turn a
/// future writer that breaks that into an omitted clause, never a false
/// figure.
struct SwitchedHeadroom {
    account: String,
    used: f64,
}

/// The new account's live 5h window percent off the disk usage cache — the
/// account-changed note's figure (r9, reviewzy entry E4). The same read class
/// as the nudge's [`headroom_of`], by name where that one takes a loaded
/// profile: [`crate::profile_json::profile_windows_for`] (the read
/// `chain_would_act` uses) plus the liveness predicate [`crate::usage::five_hour_live`]
/// it applies. `None` when there is no figure to name: no cached OAuth usage —
/// api-key and third-party accounts have no 5h pool — or a cached window that
/// has lapsed, whose percent belongs to a pool that is open again and would be
/// a false claim about "its 5h window".
fn switched_headroom_pct(account: &str) -> Option<f64> {
    let crate::profile_json::ProfileWindows::Oauth {
        usage: Some(usage), ..
    } = crate::profile_json::profile_windows_for(&crate::profile::ProfileName::from(account))
    else {
        return None;
    };
    let now = crate::usage::now_epoch_secs();
    if !crate::usage::five_hour_live(&usage, now) {
        return None;
    }
    Some(usage.five_hour.as_ref()?.utilization)
}

// ── the headroom nudge ─────────────────────────────────────────────────────

/// The nudge gate's inputs, gathered OUTSIDE the scope lock exactly like the
/// account resolution: the disk cache reads and the decision-leg replay below
/// must not run inside the read-modify-write hold.
#[derive(Clone, Copy)]
struct NudgeRead {
    /// (a): the resolved account's live 5h window plus its own threshold, the
    /// measured burn rate, and the instant it was read. `None` when the account
    /// is unattributable, carries no cached window, or its window is not live
    /// (a lapsed window means the pool is open again) — any of which answers
    /// the gate "no" rather than failing the fire.
    headroom: Option<Headroom>,
    /// (b): the decision leg's walk replayed over the disk cache — `true` when
    /// a switch, or a wrap-off sign-out, would land instead of the refusal.
    /// Recomputed only when the projection arm already passed, since it costs
    /// one cache read per chain member.
    chain_acts: bool,
}

/// The active account's live 5h window, as the gate and the renderer read it.
#[derive(Clone, Copy)]
struct Headroom {
    used: f64,
    threshold: f64,
    /// `resets_at` as epoch seconds — both the render input and the window
    /// identity the suppression state keys on.
    resets_at: i64,
    /// Measured burn %/h, `None` until enough distinct samples exist.
    rate: Option<f64>,
    /// When the window was read, so the figures are projected from one
    /// consistent instant rather than a second clock read later in the fire.
    now: i64,
}

/// The figures the approved copy's placeholders render.
#[derive(Clone, Copy)]
struct NudgeFigures {
    used: f64,
    rate: f64,
    /// Projected cap instant, epoch seconds.
    when: i64,
    /// The window's reset instant, epoch seconds.
    reset: i64,
}

/// Gather the nudge's inputs. `None` for a fire that is not nudge-eligible at
/// all: not a parent-scope `Task` call, or a session the fallback chain is
/// allowed to move. Every read here is lock-free disk IO, run before the scope
/// lock, like the account resolution it sits beside.
///
/// `shared` is the fire's one `load_config` — `run()` passes the load its
/// account-note resolve already took (or that load's failure, which the
/// reader then retries); `None` makes the reader load its own — the common
/// gate-closed fire, and the test seam.
fn read_nudge(payload: &Payload, shared: Option<&crate::profile::AppConfig>) -> Option<NudgeRead> {
    // Subagent fires are the one scope this never answers for: the reader that
    // decides to spawn again is the parent, and a shared flag would let the
    // first subagent fire consume the note.
    if payload.agent_id.is_some() || payload.tool_name.as_deref() != Some("Task") {
        return None;
    }
    // A session the chain may move already hears about the move one tool call
    // after it lands — the account note's own job. The registry row is keyed by
    // the clauth runtime sid, which this hook child reaches through the
    // `CLAUDE_CONFIG_DIR` `clauth start` sets (the payload's `session_id` is
    // Claude Code's conversation id, a different namespace). One lock-free row
    // read; no runtime dir (a bare `claude`) means no row, and no chain may
    // move a bare session either.
    if crate::which::session_config_dir()
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .and_then(crate::runtime::sid_of_runtime_dir_name)
        .and_then(|sid| crate::live_sessions::get(&sid))
        .is_some_and(|row| row.follows_chain)
    {
        return None;
    }
    let owned;
    let config = match shared {
        Some(config) => config,
        // The fire's shared load never happened (the account note's gate
        // stayed closed) or failed — and the test seam. Load afresh, exactly
        // as this reader always did.
        None => {
            owned = crate::profile::load_config().ok()?;
            &owned
        }
    };
    // The account this conversation runs on, through the same tier walk the
    // account note resolves with — never `config.state.active_profile`, which
    // a runtime session or a divergence can differ from. The walk below
    // anchors on the SAME account: `snapshot_chain`'s global-active anchor
    // would answer about a switch that never moves a pinned session.
    let (account, _) = crate::which::resolve_active(config)?;
    let anchor = crate::profile::ProfileName::from(account);
    let profile = config.find(&anchor)?;
    let headroom = headroom_of(profile);
    let chain_acts = headroom
        .as_ref()
        .is_some_and(|h| projection_arm(h).is_some())
        && chain_would_act(config, &anchor);
    Some(NudgeRead {
        headroom,
        chain_acts,
    })
}

/// The resolved account's live 5h window off the disk cache the scheduler
/// persists, plus the threshold and rate the gate and renderer need. `None` for
/// an api-key/third-party account too: its failure mode is the provider's own
/// bars, not the Anthropic 5h pool the copy names.
fn headroom_of(profile: &crate::profile::Profile) -> Option<Headroom> {
    let crate::profile_json::ProfileWindows::Oauth {
        usage: Some(usage), ..
    } = crate::profile_json::profile_windows(profile)
    else {
        return None;
    };
    let now = crate::usage::now_epoch_secs();
    if !crate::usage::five_hour_live(&usage, now) {
        return None;
    }
    let window = usage.five_hour.as_ref()?;
    let resets_at = window
        .resets_at
        .as_deref()
        .and_then(crate::usage::iso_to_epoch_secs)?;
    Some(Headroom {
        used: window.utilization,
        threshold: crate::fallback::threshold_for(profile),
        resets_at,
        rate: burn_rate(profile, window),
        now,
    })
}

/// The measured 5h burn (%/h): the durable per-profile history plus the live
/// sample, through the same recency-weighted computation and the same three
/// constants the decision leg's `fallback::burn_rate_for_profile` runs. That
/// function is private to `fallback.rs`; this is its whole body, and the burn
/// module is the shared codepath.
fn burn_rate(profile: &crate::profile::Profile, window: &crate::usage::UsageWindow) -> Option<f64> {
    let pair = ("5h", window);
    crate::usage::compute_burn_rates_from_history(
        &crate::profile::load_usage_history(&profile.name),
        std::slice::from_ref(&pair),
        crate::usage::BURN_LOOKBACK_MS,
        crate::usage::BURN_MIN_SAMPLES,
        crate::usage::BURN_GAP_CUT_MS,
    )
    .remove("5h")
    .flatten()
}

/// The projection arm's floor, half the cap. Below it the window's unspent
/// half outweighs the spent one, and the window-relative rate's early high
/// reading — the exact regime `fallback::is_exhausted_projected`'s floor guard
/// exists to bound — is the whole of the projection's base, so a fire from
/// there is noise, not a warning. The fallback's own configured floor
/// (`burn_switch_floor_pct`, default 98, band 90+) is deliberately not it: it
/// bounds a SWITCH's wasted headroom and, sitting above the default threshold,
/// clamps the projection arm out of every below-threshold window — which is
/// the whole point of the r8 gate.
const NUDGE_BURN_FLOOR_PCT: f64 = 50.0;

/// The projection arm — the rate-bearing half of the r8 gate — shared by
/// [`read_nudge`] (the chain-walk pre-gate: the walk costs one cache read per
/// member, so it runs exactly when the gate could fire) and [`nudge_figures`]
/// (the gate itself). One predicate, so the two cannot drift apart.
///
/// The active account's measured burn through `fallback::projected_exhausted`
/// — the floor-guarded arm of the decision leg's `is_exhausted_projected`,
/// shared rather than re-derived — over the seconds left until `resets_at`.
/// The horizon is the window remainder, never a poll interval (the todo's own
/// pin), so the fallback's horizon cap is not applied (`u64::MAX`); the
/// floor ([`NUDGE_BURN_FLOOR_PCT`]) is the guard's own. Emit only when the
/// projection reaches 100: the approved copy claims "it reaches its cap
/// {when}", so a fire whose rate caps the window only after the reset stays
/// silent — the constraint holds for the static threshold case too, which the
/// projection arm subsumes (`used >= threshold` implies the floor conjunct).
/// The weekly arm of the leg's predicate is deliberately not part of this
/// gate either: the note names the 5h window, so only the 5h window's own
/// projection is its premise — which leaves the copy's 429 claim uncovered for an account whose 429s come from the 7d cap, the deliberate price of that premise. `None` for a no-rate window — r7's static check, deleted
/// now, could only have ended the fire in silence anyway: see
/// [`nudge_figures`].
fn projection_arm(h: &Headroom) -> Option<f64> {
    let rate = h.rate.filter(|r| *r > 0.0)?;
    let secs_left_ms = h.resets_at.saturating_sub(h.now) as u64 * 1000;
    crate::fallback::projected_exhausted(
        h.used,
        h.threshold,
        rate,
        secs_left_ms,
        NUDGE_BURN_FLOOR_PCT,
        // The look-ahead IS the window remainder; no cap applies.
        u64::MAX,
    )
    .then_some(rate)
}

/// The gate whole — r8's projection form, replacing r7's static gate.
///
/// The projection arm emits: a rate-bearing window whose floor-guarded
/// projection reaches the cap before the reset, whether or not it sits at the
/// static threshold — the r8 upgrade over r7's `window_exhausted` gate. The
/// static threshold check itself is deleted; the todo's last sentence ("no
/// rate leaves the static threshold check alone") is honored behaviorally,
/// not structurally. A no-rate account was silent under r7 either way —
/// below the threshold the check bailed itself, at the threshold it answered
/// true and the rate filter after it silenced the emit — and so does r8's
/// gate, which a no-rate window reaches only through [`projection_arm`]'s
/// `None`: the approved copy's `{rate}`/`{when}` placeholders have no
/// figures to fill, so nothing can render, and nothing a no-rate account
/// hears changes.
fn nudge_figures(read: &NudgeRead) -> Option<NudgeFigures> {
    let h = read.headroom.as_ref()?;
    if read.chain_acts {
        return None;
    }
    let rate = projection_arm(h)?;
    let when = if h.used >= 100.0 {
        // Already at the cap: the projected instant is the present — the computation below answers identically for every input the API can produce (its utilization stays within 0..=100; at exactly 100 the seconds-to-cap is zero), so this branch is intent, never a pinnable split.
        h.now
    } else {
        // The copy's `{when}` — the cap instant at the measured rate. A figure,
        // never a gate: the arm above has already pinned it at-or-inside the
        // window, so it cannot overshoot `resets_at`. The one equality case —
        // `secs_to_cap` equalling the window remainder, a measure-zero f64
        // boundary — renders `when == reset`: the copy then says "reaches its
        // cap" at the reset instant, not before it.
        let secs_to_cap = ((100.0 - h.used) / rate * 3600.0) as i64;
        h.now.saturating_add(secs_to_cap)
    };
    Some(NudgeFigures {
        used: h.used,
        rate,
        when,
        reset: h.resets_at,
    })
}

/// The shipped copy, byte for byte — reviewzy-approved human_text (project
/// clauth, entry on this file titled "new nudge copy: headroom exhaustion
/// after an agent spawn", 2026-08-28). Never reword. The two instants render
/// through [`crate::format::local_stamp`], the crate's one LOCAL prose-stamp
/// formatter (owner ruling 2026-08-22); `None` — silence — when a stamp
/// cannot render.
fn render_nudge(f: &NudgeFigures) -> Option<String> {
    let when = crate::format::local_stamp(f.when)?;
    let reset = crate::format::local_stamp(f.reset)?;
    Some(format!(
        "clauth note: 5h window {}% used ({:.1}%/h). at this rate, it reaches its cap {}, resets {}. no fallback is set; further agent spawns may fail with 429s.",
        crate::format::format_pct(f.used).trim_end_matches('%'),
        f.rate,
        when,
        reset,
    ))
}

/// The decision leg's walk replayed over the disk cache: `true` when the daemon
/// would land a switch — or a wrap-off sign-out — instead of the refusal.
/// Anchored on `anchor` — the account the gate resolved — never the global
/// active: a pinned runtime session can resolve to a member a switch never
/// moves, and a walk anchored on the global active would answer "the chain
/// would act" about that switch
/// ([`crate::fallback::snapshot_chain_from`]). Same call the leg makes,
/// `fallback::next_auto_switch_target`, fed a store hydrated from the caches
/// the daemon's own store is persisted to and hydrated from; a member with no
/// cached OAuth usage reads exactly as it reads in the real store (absent
/// entry = headroom). The `Arc<RankedMutex>` wrapper is the entry point's
/// signature, not shared state: the mutex is process-private, never
/// contended, locked only for the walk's own snapshot clone, and taken while
/// this process holds no other rank — so no rank in the global order is
/// acquired in a context that could invert it. The walk's `fresh` and
/// `kick_rejected` channels stay empty, which the snapshot type documents as
/// "not config state": freshness is a preference the any-fresh pass renders
/// moot, and a kick-rejected member reads as headroom here where the live
/// leg would walk around it — a bounded corner this note trades for reusing
/// the leg rather than reimplementing it. The live leg's `reading_is_actionable` pre-gate is absent too (no `StatusStore` in a hook process); that omission can only SUPPRESS — a stale reading the live leg would ignore reads as actionable here, and `chain_acts` true never emits.
fn chain_would_act(
    config: &crate::profile::AppConfig,
    anchor: &crate::profile::ProfileName,
) -> bool {
    let Some(snapshot) = crate::fallback::snapshot_chain_from(config, anchor) else {
        // The resolved account is outside the chain: the leg would do
        // nothing, which is exactly "nothing would catch".
        return false;
    };
    let usage: std::collections::HashMap<String, crate::usage::UsageInfo> = snapshot
        .chain
        .iter()
        .filter_map(
            |m| match crate::profile_json::profile_windows_for(&m.name) {
                crate::profile_json::ProfileWindows::Oauth {
                    usage: Some(usage), ..
                } => Some((m.name.to_string(), *usage)),
                _ => None,
            },
        )
        .collect();
    let store: crate::usage::UsageStore =
        std::sync::Arc::new(crate::lockorder::RankedMutex::new(usage));
    crate::fallback::next_auto_switch_target(&snapshot, &store).is_some()
}

/// The nudge verdict against the state this scope remembers.
enum NudgeOutcome {
    Emit(NudgeFigures),
    /// Silent this fire. `rearm` when the verdict flipped false on a window
    /// that was told, so a later true verdict in the same window re-announces.
    Silent {
        rearm: bool,
    },
}

fn nudge_outcome(read: &NudgeRead, state: &Option<NudgeState>) -> NudgeOutcome {
    let Some(figures) = nudge_figures(read) else {
        let rearm = read.headroom.as_ref().is_some_and(|h| {
            state
                .as_ref()
                .is_some_and(|s| s.resets_at == Some(h.resets_at) && s.emitted)
        });
        return NudgeOutcome::Silent { rearm };
    };
    if state
        .as_ref()
        .is_some_and(|s| s.resets_at == Some(figures.reset) && s.emitted)
    {
        NudgeOutcome::Silent { rearm: false }
    } else {
        NudgeOutcome::Emit(figures)
    }
}

/// Decide whether this fire earns the nudge and store what it learned: its own
/// read-modify-write on the SAME record the account note uses, under the same
/// scope lock — a verdict that cannot be remembered is suppressed exactly like
/// an account note, because the record is the suppression mechanism. The
/// account note's own emit is not this function's to drop, so the two run as
/// separate lock cycles rather than one shared hold.
fn nudge_note(payload: &Payload, read: &NudgeRead) -> Option<String> {
    let path = record_path(&payload.session_id, None).ok()?;
    let _hold = ScopeLock::acquire();
    let stored = load_record(&path);
    let mut record = stored.clone().unwrap_or_default();
    if payload.transcript.is_some() {
        record.transcript = payload.transcript.clone();
    }
    match nudge_outcome(read, &record.nudge) {
        NudgeOutcome::Emit(figures) => {
            let rendered = render_nudge(&figures)?;
            record.nudge = Some(NudgeState {
                resets_at: Some(figures.reset),
                emitted: true,
            });
            if stored.as_ref() != Some(&record) && store_record(&path, &record).is_err() {
                crate::logline::to_logfile(format_args!(
                    "hook-note: cannot persist {}; staying silent",
                    path.display()
                ));
                return None;
            }
            Some(rendered)
        }
        NudgeOutcome::Silent { rearm } => {
            if rearm {
                if let Some(state) = record.nudge.as_mut() {
                    state.emitted = false;
                }
                if stored.as_ref() != Some(&record) && store_record(&path, &record).is_err() {
                    crate::logline::to_logfile(format_args!(
                        "hook-note: cannot persist {}; staying silent",
                        path.display()
                    ));
                }
            }
            None
        }
    }
}

/// Drop the records of conversations that are gone, from the same sweep that
/// reaps stale runtime trees and registry rows.
///
/// A record names its own transcript, so the test is exact rather than an age
/// guess: Claude Code deleting the transcript is the conversation ending for
/// good. The age clause below covers only a record that never carried one.
pub(crate) fn gc_conversation_records() {
    let Ok(dir) = records_dir() else {
        return;
    };
    // Peek BEFORE locking, the way `gc_bare_markers` does and for the same
    // reason: this runs at every `clauth mcp` boot and every `clauth start`, so
    // nothing to sweep must not pay an acquisition. It also keeps the
    // acquisition's `mkdir_700` off a box where the hook has never fired, which
    // would otherwise grow a records dir and a lock file from a sweep alone.
    //
    // The early return covers a VIRGIN tree only: `.lock` is permanent once any
    // hook has fired, so a box with zero records still counts one entry and
    // still pays. Sub-ms uncontended, and the same shape the sibling has.
    let Ok(mut peek) = std::fs::read_dir(&dir) else {
        return;
    };
    if peek.next().is_none() {
        return;
    }
    // Under the same hold the writers take. Without it the sweep unlinks the
    // very files `ScopeLock` serialises, so a reap landing inside a fire's
    // read-modify-write drops that write on the floor.
    //
    // What the lock does NOT cover, measured rather than reasoned: the reap
    // can cost a live scope its baseline with ZERO concurrency involved
    // (baseline, move, sweep, fire — the fire finds no record and rebaselines,
    // swallowing that one account change). 40 concurrent trials against a
    // fresh record announced 40/40; against a reap-eligible one, 0/40. The
    // loss is caused by the reap predicate's INPUT, not by any interleave, so
    // this lock was never going to cover it. The guard lives in the writer:
    // every fire moves its record's mtime ([`touch_record`]), so `age` below
    // means time since this scope last FIRED, and a conversation still firing
    // can never elapse the grace, whatever its transcript's state. The
    // deletion stays self-undoing — the fire recreates the record immediately
    // — which is what bounds a fire that neither wrote nor touched.
    let _hold = ScopeLock::acquire();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Records only. The dir also holds the `.lock` file and, for an instant,
        // an `atomic_write_600` temp; reaping either would be a sweep deleting
        // live machinery.
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let age = || {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
        };
        let reap = match load_record(&path).and_then(|r| r.transcript) {
            // Grace, not a bare `!exists()`: a baseline recorded at
            // `SessionStart` can land before Claude Code creates the transcript,
            // and reaping it there loses the baseline, so the conversation's next
            // real move is absorbed as a first fire and never announced.
            Some(transcript) => {
                !transcript.exists() && age().is_some_and(|a| a > MISSING_TRANSCRIPT_GRACE)
            }
            None => age().is_some_and(|a| a > ORPHAN_RECORD_MAX_AGE),
        };
        if reap {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/hook_note.rs"]
mod tests;
