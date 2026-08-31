//! Per-session `CLAUDE_CONFIG_DIR` trees used by `clauth start`.
//!
//! Under real symlinks every `clauth start <profile>` session gets its OWN
//! runtime tree, keyed by a session id (`<pid>-<seq>`):
//! `~/.clauth/profiles/<profile>/runtime-<sid>/`, or `runtime-isolated-<sid>/`
//! for an isolated run. Without them the tree is shared per profile+flavor
//! instead — see the keying rule below. Its `.credentials.json` still
//! resolves to the profile's canonical creds, so concurrent sessions of one
//! profile observe a single chain of refresh tokens. A watchdog thread in each
//! parent process keeps the runtime tree and canonical state in sync.
//!
//! The layout rests on ONE rule, which every enumeration below applies instead
//! of a hardcoded name list: a runtime dir named `runtime<rest>` pairs with the
//! sessions dir named `sessions<rest>`. It covers both flavors and also the
//! pre-per-session `runtime`/`sessions` pair an earlier release left on disk, so
//! liveness and GC reach a legacy tree with no migration step.
//!
//! Per-session keying is for REAL symlinks only. Under [`LinkMode::Fake`] the
//! tree is a recursive copy, so both flavors fall back to the bare stem
//! ([`paired_dir_names`]) and every session of that profile+flavor shares one
//! tree. The accepted consequence is that Windows without symlink privilege
//! cannot host independent per-session credentials.
//!
//! Two transport modes, probed per profile at acquire time BEFORE the tree name
//! is chosen, since the mode decides that name:
//!
//! - **Real symlinks** (Unix, plus Windows with developer mode or admin):
//!   the runtime tree is a forest of symlinks into `~/.claude/`, and
//!   `.credentials.json` is a symlink into the profile's canonical creds.
//!   The watchdog only repairs the `.credentials.json` link when Claude
//!   Code's `unlink + write` re-login replaces it with a regular file.
//!
//! - **Fake symlinks** (Windows without symlink privilege): the runtime
//!   tree is built by recursive copy, and `.credentials.json` is a regular
//!   file. The watchdog walks both sides every tick and reconciles by
//!   "latest mtime wins" so a re-login on either side propagates to the
//!   other before another session can pick up a stale refresh token.
//!
//! Liveness lives in the paired sessions directory: the session creates the
//! marker `<sessions dir>/<sid>` and holds an exclusive `flock(2)` on it for
//! its lifetime, so any other process reads liveness without cooperation.
//! [`has_live_session`] unions every `sessions*` dir under the profile, so an
//! isolated session counts the same as a shared one; the destructive account
//! actions (delete, disable) gate on it everywhere. Token rotation gates on it
//! only on macOS ([`rotation_blocked_for`]): elsewhere the session reads the
//! very credential file a rotation writes and simply follows it, while on macOS
//! its Claude Code reads a Keychain item namespaced per `CLAUDE_CONFIG_DIR` that
//! clauth cannot write. Teardown drops the marker and discards the tree —
//! its own under real symlinks, the shared one under [`LinkMode::Fake`] and only
//! once the last session of the profile has left; [`gc_stale_runtimes`] collects
//! what a crashed session left behind, of either flavor and in either layout.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::claude::{build_claude_settings_json, create_symlink};
use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::{
    ClaudeCredentials, Profile, ProfileName, atomic_write_600, claude_dir, clauth_dir, home_dir,
    profile_dir, profile_subpath,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkMode {
    /// OS-level symlinks. Used on Unix unconditionally and on Windows when
    /// the process can create symlinks (developer mode or admin).
    Real,
    /// Bidirectional mtime-based mirror. Used on Windows when the OS denies
    /// symlink creation.
    Fake,
}

/// What [`link_mode_of`] observed: one verdict per probe shape, so the MCP
/// note states the transport it actually saw rather than hedging over every
/// possibility or guessing off one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkProbe {
    /// Both shared entries are symlinks: the real transport.
    Real,
    /// Both shared entries are plain files or dirs: the copy transport.
    Fake,
    /// Neither shared entry exists: no mirror paths to describe.
    NothingShared,
    /// The entries disagree: one links, the other is a copy.
    Mixed,
}

/// The transport an EXISTING runtime tree was built with, read off the entries
/// the tree shares with `~/.claude` (`CLAUDE.md` and `skills`). A link means
/// [`LinkProbe::Real`], a plain file or dir means [`LinkProbe::Fake`], neither
/// entry existing means [`LinkProbe::NothingShared`], and the two entries
/// disagreeing means [`LinkProbe::Mixed`]. The sibling of the acquire-time
/// privilege probe [`detect_link_mode`], which tests what THIS process may
/// create; this one observes the tree already in front of a later process. The
/// MCP instructions block states the probe's answer instead of spelling both
/// transports every session. Costs two stats at most, so callers re-run it per
/// reply rather than caching.
///
/// `Mixed` must not resolve to one entry's verdict: the real-mode watchdog
/// repairs only `.credentials.json`, so a rename-replace edit of `CLAUDE.md`
/// (atomic-save editors, the model's own tooling — the note itself invites
/// editing it) permanently swaps that entry's link for a plain file on a
/// symlink host. A probe trusting the first entry would then state the wrong
/// transport and the wrong new-file rule, so disagreement names both
/// transports instead, which is true under either. With one entry present,
/// its verdict stands: there is nothing else to check it against. A missing
/// config dir reads `NothingShared`: there is no tree to describe.
pub(crate) fn link_mode_of(config_dir: Option<&Path>) -> LinkProbe {
    let Some(dir) = config_dir else {
        return LinkProbe::NothingShared;
    };
    let mut verdict: Option<LinkProbe> = None;
    for entry in ["CLAUDE.md", "skills"] {
        let Ok(meta) = std::fs::symlink_metadata(dir.join(entry)) else {
            continue;
        };
        let seen = if meta.file_type().is_symlink() {
            LinkProbe::Real
        } else if meta.is_file() || meta.is_dir() {
            LinkProbe::Fake
        } else {
            continue;
        };
        match verdict {
            None => verdict = Some(seen),
            Some(prev) if prev == seen => {}
            Some(_) => return LinkProbe::Mixed,
        }
    }
    verdict.unwrap_or(LinkProbe::NothingShared)
}

/// Whether a session inherits the operator's full `~/.claude/` (memory,
/// plugins, hooks, commands, agents) or runs authenticated-but-clean. Both
/// flavors are keyed identically (see [`paired_dir_names`]); the flavor decides
/// only what is materialized into the tree, since every session shares the
/// profile's canonical credentials and rotation lock either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Isolation {
    /// Full mirror of `~/.claude/`: the session behaves like the operator's.
    Shared,
    /// Credentials injected, but operator memory/plugins/hooks/commands/agents
    /// omitted and settings built from an empty base — no house style leaks.
    Isolated,
}

/// Directory-name stems. A per-session dir appends `-<sid>`; the bare stem is
/// the pre-per-session layout, which the pairing rule still covers.
const RUNTIME_STEM: &str = "runtime";
const SESSIONS_STEM: &str = "sessions";
const ISOLATED_RUNTIME_STEM: &str = "runtime-isolated";
const ISOLATED_SESSIONS_STEM: &str = "sessions-isolated";

impl Isolation {
    fn runtime_stem(self) -> &'static str {
        match self {
            Isolation::Shared => RUNTIME_STEM,
            Isolation::Isolated => ISOLATED_RUNTIME_STEM,
        }
    }
    fn sessions_stem(self) -> &'static str {
        match self {
            Isolation::Shared => SESSIONS_STEM,
            Isolation::Isolated => ISOLATED_SESSIONS_STEM,
        }
    }
}

/// Per-process counter making each `acquire`'s [`SessionId`] unique. A single
/// process can hold several live sessions of the same profile+flavor at once —
/// the `clauth mcp` server firing overlapping `delegate`s. Keying only on the
/// pid would make the second acquire block forever on the first's `flock(2)` (an
/// exclusive lock on a second fd of the same path waits), hanging the delegate in
/// `acquire` with no session ever spawned.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// How many times `acquire` re-mints a [`SessionId`] whose marker a live holder
/// already owns. The counter above makes an in-process collision impossible, so
/// a holder here is always another PROCESS that minted the same `<pid>-<seq>`,
/// and its own counter has to keep pace with ours for a re-mint to collide
/// again. A handful of attempts outruns that; exhausting them is a real anomaly
/// and fails loudly rather than waiting.
const SID_COLLISION_REMINTS: u32 = 8;

/// A session's process-unique id, `<pid>-<seq>`: the name of its liveness marker
/// file AND the suffix keying its own runtime + sessions dirs. Digits and one
/// `-` only, which is what makes a session id unable to spell the `isolated`
/// flavor stem — the property [`is_shared_runtime_dir_name`] relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionId(String);

impl SessionId {
    /// Mint the next id for this process. Private: an id exists only because a
    /// session was acquired, so nothing else can conjure one.
    fn mint() -> Self {
        Self(format!(
            "{}-{}",
            std::process::id(),
            SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// True iff `s` has the `<pid>-<seq>` shape [`SessionId::mint`] produces. The
/// registry validates ids it reads back off disk against this before joining
/// them into a path.
pub(crate) fn is_session_id(s: &str) -> bool {
    s.split_once('-').is_some_and(|(pid, seq)| {
        !pid.is_empty()
            && !seq.is_empty()
            && pid.bytes().all(|b| b.is_ascii_digit())
            && seq.bytes().all(|b| b.is_ascii_digit())
    })
}

/// True when `name` is one of the four shapes clauth gives a paired dir with
/// this stem: the legacy `<stem>` and `<stem>-isolated`, or the per-session
/// `<stem>-<sid>` and `<stem>-isolated-<sid>`. The single parser behind every
/// strict name check below, so the flavors and the two layouts cannot drift
/// apart across them.
fn is_paired_dir_name(name: &str, stem: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem) else {
        return false;
    };
    let rest = rest.strip_prefix("-isolated").unwrap_or(rest);
    rest.is_empty() || rest.strip_prefix('-').is_some_and(is_session_id)
}

/// True for a runtime dir name of EITHER flavor. This is the predicate GC gates
/// on, and GC `remove_dir_all`s what it matches — so it is the strict form. The
/// loose `runtime<rest>` split that [`paired_sessions_name`] uses would hand the
/// sweep anything a future release happens to name `runtime*`.
fn is_runtime_dir_name(name: &str) -> bool {
    is_paired_dir_name(name, RUNTIME_STEM)
}

/// Sibling of [`is_runtime_dir_name`] for marker dirs.
fn is_sessions_dir_name(name: &str) -> bool {
    is_paired_dir_name(name, SESSIONS_STEM)
}

/// True for a SHARED runtime dir name — a per-session `runtime-<sid>` or the
/// legacy bare `runtime` — and false for the isolated flavor or any unrelated
/// name. Callers that must reach only shared copies (both config reconcilers,
/// `clauth which`) key on this rather than an exact name.
pub(crate) fn is_shared_runtime_dir_name(name: &str) -> bool {
    is_runtime_dir_name(name) && !name.starts_with(ISOLATED_RUNTIME_STEM)
}

/// The sid of a PER-SESSION runtime dir name (`runtime-<sid>` or the isolated
/// flavor's), `None` for the legacy bare stems and unrelated names. The same
/// strict family as [`is_runtime_dir_name`], so the split cannot drift apart
/// from the predicate GC deletes by. Used by the hook-note's headroom nudge,
/// which reaches its own live-session registry row through the sid it derives
/// from its `CLAUDE_CONFIG_DIR`.
pub(crate) fn sid_of_runtime_dir_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix(RUNTIME_STEM)?;
    let rest = rest.strip_prefix("-isolated").unwrap_or(rest);
    rest.strip_prefix('-')
        .filter(|s| is_session_id(s))
        .map(str::to_string)
}

/// The sessions dir paired with a runtime dir of this name, per the module's one
/// layout rule: `runtime<rest>` ↔ `sessions<rest>`. Deliberately loose about
/// what `<rest>` is; callers that DELETE gate on [`is_runtime_dir_name`] first.
fn paired_sessions_name(runtime_name: &str) -> Option<String> {
    runtime_name
        .strip_prefix(RUNTIME_STEM)
        .map(|rest| format!("{SESSIONS_STEM}{rest}"))
}

/// Inverse of [`paired_sessions_name`].
fn paired_runtime_name(sessions_name: &str) -> Option<String> {
    sessions_name
        .strip_prefix(SESSIONS_STEM)
        .map(|rest| format!("{RUNTIME_STEM}{rest}"))
}

/// The `(runtime, sessions)` dir names a session of this flavor uses under this
/// transport. Returned as a pair so the module's `runtime<rest>` ↔
/// `sessions<rest>` rule is structural rather than two call sites agreeing.
///
/// [`LinkMode::Real`] keys each session's pair by its own `<sid>`, so sessions
/// are independent. [`LinkMode::Fake`] returns the BARE stem, shared by every
/// session of the profile+flavor: that tree is built by recursive COPY of
/// `~/.claude/`, so per-session keying charges sessions 2..N a full copy each,
/// multiple GB apiece on a real install. Disk is the whole reason — the fake-mode
/// watchdog walk is NOT: `acquire` spawns one per `ProfileRuntime` either way, so
/// N sessions perform N walks per second under both keyings, and sharing only
/// converges them on one destination tree. The accepted cost is that a
/// fake-symlink host cannot give its sessions independent credentials.
///
/// `session` is a [`SessionId`]'s string — digits and one `-`, which is what
/// keeps a per-session name from spelling the `isolated` flavor stem.
fn paired_dir_names(isolation: Isolation, session: &str, mode: LinkMode) -> (String, String) {
    let suffix = match mode {
        LinkMode::Real => format!("-{session}"),
        LinkMode::Fake => String::new(),
    };
    (
        format!("{}{suffix}", isolation.runtime_stem()),
        format!("{}{suffix}", isolation.sessions_stem()),
    )
}

/// Every path one session addresses, resolved as a unit once its [`LinkMode`] is
/// known — the mode decides the dir names, so nothing here can be computed
/// before the probe.
struct SessionPaths {
    runtime: PathBuf,
    sessions: PathBuf,
    pid_file: PathBuf,
    /// The upgrade-compat marker, and `None` when the session's own `pid_file`
    /// already sits at that path — which is exactly the shared-tree case. See
    /// [`stamp_legacy_marker`].
    legacy_marker: Option<PathBuf>,
}

impl SessionPaths {
    fn resolve(
        name: &ProfileName,
        isolation: Isolation,
        session: &SessionId,
        mode: LinkMode,
    ) -> Result<Self> {
        let (runtime_name, sessions_name) = paired_dir_names(isolation, session.as_str(), mode);
        let sessions = profile_subpath(name, &sessions_name)?;
        let pid_file = sessions.join(session.as_str());
        // The PRE-per-session marker path, `<profile>/sessions[-isolated]/<sid>`.
        let legacy = profile_subpath(name, isolation.sessions_stem())?.join(session.as_str());
        Ok(Self {
            runtime: profile_subpath(name, &runtime_name)?,
            legacy_marker: (legacy != pid_file).then_some(legacy),
            sessions,
            pid_file,
        })
    }
}

/// Stamp and hold this session's upgrade-compat marker, returning the fd whose
/// flock is the liveness signal.
///
/// A clauth process built before the per-session layout probes exactly
/// `<profile>/sessions` and `<profile>/sessions-isolated`. It cannot see a
/// `sessions-<sid>/` dir, so its `has_live_session` reads a live new-layout
/// session as idle and its rotation leg spends the single-use refresh token that
/// session still holds — the chain dies and the account needs a re-login. That is
/// the DEFAULT state right after an upgrade: `clauth daemon --replace` exists
/// precisely because the running daemon is otherwise the old binary until the
/// next restart.
///
/// Under the shared bare-stem tree the session's OWN marker already sits at that
/// path, so a second marker would be the same file: `open` + `try_lock` from this
/// process conflicts with the fd `acquire` holds and fails. Hence
/// [`SessionPaths::legacy_marker`] is an `Option` — there is no second marker to
/// stamp, and this is unreachable rather than failing every start.
///
/// Best-effort: failing to stamp costs upgrade safety, not the session, so it is
/// logged and stepped over rather than propagated.
///
/// Ceiling: an upgrade-window shim, added 2026-07-25. Delete it — with
/// `ProfileRuntime::legacy_marker` and the count dedupe it forces in
/// `live_session_count` (unlinked: that one is `cfg(test)` now, so an intra-doc
/// link to it would not resolve in a doc build) — a few releases after the
/// per-session layout ships, once no pre-layout binary can still be supervising
/// a live install.
fn stamp_legacy_marker(path: &Path) -> Option<File> {
    let dir = path.parent()?;
    if let Err(e) = crate::profile::mkdir_700(dir) {
        logline!(
            "clauth: upgrade-compat marker dir {} failed: {e}",
            dir.display()
        );
        return None;
    }
    let file = match open_pid_file(path) {
        Ok(file) => file,
        Err(e) => {
            logline!(
                "clauth: upgrade-compat marker {} failed: {e}",
                path.display()
            );
            return None;
        }
    };
    // `try_lock`, not `lock`. The DIR here is shared across the profile's
    // sessions but this FILE is `sessions/<sid>`, so contention needs a second
    // live process that minted the same `<pid>-<seq>` — a shared `~/.clauth`
    // across pid namespaces, or an NFS home. Rare, and a blocking wait would hang
    // `acquire` inside the state lock; a `None` here is also what keeps teardown
    // from unlinking a marker this session never owned.
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(e) => {
            logline!(
                "clauth: upgrade-compat marker {} not lockable: {e}",
                path.display()
            );
            None
        }
    }
}

/// Both paths at which the session a registry row names could hold its liveness
/// marker. The layout lives here so no other module rebuilds it —
/// `crate::live_sessions` tests a row's liveness through this, and would go
/// silently stale if it spelled the path itself.
///
/// A row carries the profile, the flavor, and the session id but NOT the
/// transport, and the two layouts put the marker in different dirs. So both are
/// derived from [`paired_dir_names`] and a caller treats the row as live if
/// EITHER is held — the fail-safe direction, matching [`session_marker_dirs`]'s
/// deliberately loose filter: a row reaped under a live session is a live
/// session nothing can be pointed at again, while probing an absent path costs
/// one `open` that fails.
fn session_marker_paths(
    profile: &ProfileName,
    isolated: bool,
    session_id: &str,
) -> Result<[PathBuf; 2]> {
    let isolation = if isolated {
        Isolation::Isolated
    } else {
        Isolation::Shared
    };
    let marker = |mode| -> Result<PathBuf> {
        let (_, sessions_name) = paired_dir_names(isolation, session_id, mode);
        Ok(profile_subpath(profile, &sessions_name)?.join(session_id))
    };
    Ok([marker(LinkMode::Real)?, marker(LinkMode::Fake)?])
}

/// Whether the session a registry row names is still running. `true` on anything
/// the probe could not decide, keeping [`is_session_alive`]'s direction: a row
/// wrongly read as live costs one wasted registry write, while one wrongly read as
/// dead silently freezes that session out of the chain (or, for GC, reaps a live
/// session's row).
///
/// Callers choose WHICH profile to probe: the tally, GC, and the decision leg all
/// probe `current_member` first (where a swapped session holds its markers) and
/// fall back to `start_profile` (where a session that never moved lives), so a row
/// can never be alive for one consumer and dead for another. A caller MUST use the
/// same fallback the tally uses, or GC would reap what the tally counts.
pub(crate) fn session_row_is_live(
    start_profile: &ProfileName,
    isolated: bool,
    session_id: &str,
) -> bool {
    let Ok(markers) = session_marker_paths(start_profile, isolated, session_id) else {
        return true;
    };
    markers.iter().any(|marker| is_session_alive(marker))
}

/// Stamp and hold the marker [`session_row_is_live`] probes, so a test can give a
/// registry row a live session without spawning one. In here rather than in the
/// test module because the marker layout lives in this file and nothing else may
/// rebuild it.
#[cfg(test)]
pub(crate) fn hold_session_row_marker(
    start_profile: &ProfileName,
    isolated: bool,
    session_id: &str,
) -> Result<File> {
    // [0] is the per-session (`LinkMode::Real`) layout — what `acquire` stamps.
    let path = session_marker_paths(start_profile, isolated, session_id)?
        .into_iter()
        .next()
        .context("session_marker_paths yielded no path")?;
    if let Some(dir) = path.parent() {
        crate::profile::mkdir_700(dir)?;
    }
    let file = open_pid_file(&path)?;
    file.try_lock()
        .with_context(|| format!("marker {} already held", path.display()))?;
    Ok(file)
}

fn profiles_root_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles"))
}

/// Every marker dir under the profile: each live session's own
/// `sessions[-isolated]-<sid>`, the legacy bare `sessions`/`sessions-isolated`,
/// and the upgrade-compat markers [`stamp_legacy_marker`] puts in the latter.
///
/// `None` when the profile dir could not be enumerated for any reason OTHER than
/// being absent — a caller must read that as "cannot rule out a live session".
/// Unlike the old fixed `<profile>/sessions` probe, this dir exists for every
/// profile that was ever configured, so its unreadability is not the idle case;
/// a transient EMFILE that read as "no sessions" would let a delete or disable
/// through against a running session.
///
/// The filter stays the LOOSE prefix test on purpose, where GC's uses the strict
/// [`is_sessions_dir_name`]: a name this misses is a live session the destructive
/// guards cannot see, while a name GC's misses is only a dir left uncollected.
fn session_marker_dirs(name: &ProfileName) -> Option<Vec<PathBuf>> {
    let profile = profile_dir(name).ok()?;
    let entries = match std::fs::read_dir(&profile) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(SESSIONS_STEM))
        {
            dirs.push(entry.path());
        }
    }
    Some(dirs)
}

/// True iff the profile has at least one live `clauth start` session, in ANY of
/// its marker dirs and of either flavor. Gates the destructive account actions
/// (delete, disable) everywhere, and on macOS every rotation leg too
/// ([`rotation_blocked_for`]).
///
/// Every unknown reads as LIVE, and that asymmetry is no longer free. A false
/// negative pulls an account out from under a running session or signs it out.
/// A spurious true costs a refused delete the user can retry — but on macOS it
/// ALSO freezes all four rotation legs for that profile, so a marker dir that
/// stays unreadable means the chain never rotates and the access token simply
/// dies at its 8h mark. Fail-closed is still the right direction (a wrongly
/// deleted account is unrecoverable, an unrotated one is not), but the cost is
/// a stalled profile, not an inconvenience. Only a dir that is genuinely absent
/// counts as idle.
pub(crate) fn has_live_session(name: &ProfileName) -> bool {
    match session_marker_dirs(name) {
        None => true,
        Some(dirs) => dirs
            .iter()
            .any(|dir| live_sessions_at(dir).is_none_or(|n| n > 0)),
    }
}

/// Whether a live `clauth start` session must block rotating this profile's
/// chain. Kept PURE (the caller reads `cfg!`) so both arms run from a Linux
/// test, like [`swap_support`].
///
/// macOS only, and NOT for the double-spend reason the pre-2026-07-26 code gave
/// — a double-spend costs one failed request, not the account. The real
/// mechanism is that clauth cannot reach the credential a `clauth start`
/// session's Claude Code actually reads. That child runs with
/// `CLAUDE_CONFIG_DIR=<runtime>`, and CC namespaces its Keychain item per config
/// dir (`Claude Code-credentials-<sha256(dir)[0:8]>`)
/// while [`crate::keychain`] writes only the UNSUFFIXED `Claude Code-credentials`.
/// So a rotation leaves that CC holding the old refresh token; its own
/// re-read-and-compare sees its item unchanged and detects no race, and the
/// `invalid_grant` that follows passes its CAS guard — which also compares
/// against its own item — so it BLANKS the pair and signs the session out
/// mid-task.
///
/// A bare `claude` is deliberately NOT covered: it reads the unsuffixed item
/// clauth does write, so rotating it propagates normally. [`has_live_session`]
/// counts only `clauth start` sessions, so it already draws that line.
///
/// This dissolves the moment [`crate::keychain`] derives the namespaced service
/// name. Every site that refuses goes
/// through [`rotation_blocked_for`], so that fix is a one-line change here and
/// nowhere else — keep it that way rather than re-deriving `cfg!(macos) &&
/// has_live_session` at a call site.
fn rotation_blocked_by_live_session(has_live_session: bool, is_macos: bool) -> bool {
    is_macos && has_live_session
}

/// Whether ANY live `clauth start` session for `name` launched on a credential
/// that carries a refresh token — the only kind a rotation can strand.
///
/// [`rotation_blocked_by_live_session`] spells out why a live session blocks
/// rotation on macOS: that session's Claude Code holds the pair in a Keychain
/// item clauth cannot write, so a rotation leaves it spending a superseded
/// refresh token, and the `invalid_grant` that follows blanks its item and
/// signs the session out mid-task. Every step of that mechanism needs a refresh
/// token to attempt. A session launched on a `session-token.json` sidecar has
/// none — CLA-SPLIT put it there exactly so sessions hold nothing rotatable —
/// so there is nothing for it to spend and nothing for a rotation to strand.
///
/// Deliberately feature-agnostic: it asks what the session HOLDS, never
/// whether the rolling token is enabled. An upstream #53 `claude setup-token` mint answers the
/// same way a rolling token does, and gets the same exemption for the same
/// reason, which is why this is a narrowing of the refusal rather than a
/// carve-out bolted beside it.
///
/// Read at ROTATION time and keyed on the row's PATH, not on a verdict frozen
/// at launch: the content at that path can change under a running session
/// ([`crate::claude::heal_misfilled_sidecar`] exists because a rotating pair
/// can land in a sidecar), and a frozen bool would keep saying "refresh-less"
/// while the file the session reads holds a live chain.
///
/// EVERY unknown reads as rotatable, matching [`has_live_session`]'s own
/// fail-closed asymmetry: an unreadable marker dir, a marker with no registry
/// row (`acquire` tolerates a failed registration), a row from a clauth that
/// predates `launch_store`, and an unreadable or half-written credential file
/// all return `true` and refuse exactly as today. The one readable shape that
/// ALLOWS is a file that parses with no `claudeAiOauth` block at all (`{}`):
/// it holds no refresh token to strand, so permitting the rotation is the
/// verdict, not a hole in the enumeration. Bare `claude` sessions never
/// reach this predicate at all — their stand-in markers live under
/// [`live_bare_dir`], not the profile — so the refusal is unchanged for them
/// by construction rather than by this check.
fn live_session_holds_rotatable(name: &ProfileName) -> bool {
    let Some(dirs) = session_marker_dirs(name) else {
        return true;
    };
    for dir in &dirs {
        let Some(ids) = live_marker_names(dir) else {
            return true;
        };
        for id in ids {
            let Some(session_id) = id.to_str() else {
                return true;
            };
            let Some(store) = crate::live_sessions::get(session_id).and_then(|r| r.launch_store)
            else {
                return true;
            };
            let refreshless =
                crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&store)
                    .ok()
                    .is_some_and(|c| c.refresh_token().is_none());
            if !refreshless {
                return true;
            }
        }
    }
    false
}

/// [`rotation_blocked_by_live_session`] against the live host and marker state —
/// what every rotation leg and both TUI pre-refusals call.
///
/// `cfg!` is tested FIRST so the marker probe short-circuits away off macOS:
/// [`has_live_session`] is a `read_dir` plus an `open` + `try_lock` per marker,
/// and passing it as an argument would pay that on every Linux poll of every
/// profile for a value the predicate discards.
///
/// [`live_session_holds_rotatable`] is tested LAST for the same reason: it is
/// strictly the more expensive probe (a registry read plus a credential parse
/// per live session), and it only ever narrows an answer that is already
/// `true`, so it is never paid by a profile that was not about to be refused.
pub(crate) fn rotation_blocked_for(name: &ProfileName) -> bool {
    cfg!(target_os = "macos")
        && rotation_blocked_by_live_session(has_live_session(name), true)
        && live_session_holds_rotatable(name)
}

/// Count of live `clauth start` sessions for the profile, deduped by marker NAME
/// across every marker dir: one session holds its own `sessions-<sid>/<sid>` and
/// an upgrade-compat `sessions/<sid>`, and the shared session id is what makes
/// those one session rather than two. Reports 1 on an unknown, so it never
/// contradicts [`has_live_session`] within a tick.
///
/// TEST-ONLY since 2026-07-25. Its one production consumer, the Plugin tab's
/// fleet tally, moved to `live_sessions::LiveTally`: this dedupes markers WITHIN
/// a profile but not across them, so a session that swapped A→B read as two
/// sessions on two accounts, and only the registry can tell those apart.
///
/// Kept rather than deleted because the marker-layout tests need the COUNT, and
/// [`has_live_session`] cannot supply it — it is a boolean `.any()`, so it reads
/// one session and two identically, which is exactly the distinction phase 0b's
/// two-sessions-on-one-profile keying rests on. Same shape as
/// [`hold_session_row_marker`]: a test-only observation of a layout this module
/// owns, so nothing outside it rebuilds the paths.
#[cfg(test)]
pub(crate) fn live_session_count(name: &ProfileName) -> usize {
    let Some(dirs) = session_marker_dirs(name) else {
        return 1;
    };
    let mut ids: HashSet<std::ffi::OsString> = HashSet::new();
    for dir in &dirs {
        match live_marker_names(dir) {
            Some(names) => ids.extend(names),
            None => return 1,
        }
    }
    ids.len()
}

/// Names of the markers currently flock-held in `sessions`. `None` when the dir
/// could not be read for any reason other than being absent, or an entry could
/// not be read — never fold either into a zero, per [`has_live_session`].
///
/// Read-only (unlike [`prune_stale_sessions`], it drops nothing), so it needs no
/// state lock — a caller reading its own dir always counts ITSELF, since a second
/// fd's `try_lock` conflicts with the one `acquire` holds.
fn live_marker_names(sessions: &Path) -> Option<Vec<std::ffi::OsString>> {
    let entries = match std::fs::read_dir(sessions) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        if is_session_alive(&entry.path()) {
            names.push(entry.file_name());
        }
    }
    Some(names)
}

/// How many live sessions hold a marker in `sessions`. `Some(0)` only when the
/// dir is genuinely absent; `None` when the probe could not tell. Callers choose
/// which way an unknown falls, because the safe direction differs: the rotation
/// gate must read it as live, and so must anything moving state out of a runtime.
pub(crate) fn live_sessions_at(sessions: &Path) -> Option<usize> {
    live_marker_names(sessions).map(|names| names.len())
}

/// Liveness markers standing in for BARE `claude` sessions — the ones started
/// without `clauth start`, reading the `~/.claude/.credentials.json` link clauth
/// owns. One flock-held `<pid>` file per `clauth mcp` server that reads those
/// global credentials ([`live_bare_sessions`] states how tight that stand-in is),
/// deliberately OUTSIDE `profiles/`:
/// [`session_marker_dirs`] scans a profile dir for names starting with
/// `SESSIONS_STEM`, so nothing here can reach [`has_live_session`] and the
/// delete, disable, and macOS rotation gates keep counting `clauth start`
/// sessions only. A bare session holds no credential clauth handed it and none
/// it could not already read, so it is a display fact, not a gate.
fn live_bare_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("live_bare"))
}

/// Stamp a marker for THIS process; the flock is held for exactly as long as the
/// returned `File` is, so any death — SIGKILL included — releases it with no
/// teardown path to run.
///
/// Keyed by pid, which the OS reuses: the file is opened without truncation and
/// re-locked, never read as dead from its name alone. The state lock is what
/// separates this create-then-lock from [`gc_bare_markers`]'s prune, which
/// unlinks whatever it reads as unlocked — a marker pruned in that window leaves
/// a running session holding an unlinked file that nothing can count.
pub(crate) fn register_bare_session() -> Result<File> {
    let dir = live_bare_dir()?;
    let path = dir.join(std::process::id().to_string());
    with_state_lock(|_held| {
        crate::profile::mkdir_700(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let file =
            open_pid_file(&path).with_context(|| format!("failed to open {}", path.display()))?;
        file.try_lock()
            .with_context(|| format!("marker {} already held", path.display()))?;
        Ok(file)
    })
}

/// How many marker holders are running. `None` when the probe could not tell,
/// exactly as [`live_sessions_at`] defines it; the caller picks the direction,
/// and the one caller that exists picks zero (see
/// [`crate::live_sessions::LiveTally::collect`]).
///
/// This is a `clauth mcp` count STANDING IN for a bare `claude` count, and the
/// approximation is loose in both directions. Under: a `claude` with no clauth
/// MCP server wired boots none, so it is counted nowhere. Over: a plugin install
/// and a manual `mcpServers.clauth` entry are namespaced separately by Claude
/// Code and coexist after an upgrade (`plugin_global` and `manual_global` are
/// independent in `tui::app`, and the tab stops offering the fix once either
/// wires it), so one session can boot two servers and render as two; and any
/// non-Claude-Code MCP client pointed at `clauth mcp` renders as a bare `claude`,
/// since nothing here reads who the client says it is.
pub(crate) fn live_bare_sessions() -> Option<usize> {
    live_sessions_at(&live_bare_dir().ok()?)
}

/// Best-effort sweep removing runtime trees whose owning session died without
/// running teardown (SIGKILL/crash strands the pair). With one tree per session
/// this is load-bearing, not housekeeping: every crashed session would otherwise
/// leak a 0600 `.claude.json` carrying that account's billing caches, forever.
///
/// Enumerates the real subdirs of each profile and pairs them by name, so it
/// reaches per-session dirs of both flavors and the legacy pre-upgrade pair
/// alike. Safe at any entry point: each removal re-checks liveness under the
/// state lock (the same teardown gate `Drop` uses), so a live session — or one
/// mid-acquire holding the lock — is never collected.
///
/// The marker sweeps are siblings of the tree sweep rather than its tail: an
/// unreadable `profiles/` says nothing about a registry row, a bare session's
/// marker, or a conversation record, and folding them in would have skipped
/// every one of them on that return.
pub(crate) fn gc_stale_runtimes() {
    gc_runtime_trees();
    gc_live_session_rows();
    gc_bare_markers();
    crate::hook_note::gc_conversation_records();
}

fn gc_runtime_trees() {
    let Ok(root) = profiles_root_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for profile in entries.flatten() {
        let profile = profile.path();
        let Ok(children) = std::fs::read_dir(&profile) else {
            continue;
        };
        for child in children.flatten() {
            let file_name = child.file_name();
            let Some(child_name) = file_name.to_str() else {
                continue;
            };
            // Strict predicates, not the loose pairing split: this loop hands
            // `remove_dir_all` whatever it matches, so a future `runtime_state`
            // or `sessions.json` under a profile must fall through untouched.
            if is_runtime_dir_name(child_name)
                && let Some(sessions) = paired_sessions_name(child_name)
            {
                // A stale pre-upgrade `runtime/` pairs with the same `sessions/`
                // the compat markers live in, so it is spared while ANY session of
                // the profile runs and only collected once the last one leaves.
                // Delayed cleanup, not a leak.
                let _ = gc_one_pair(&child.path(), &profile.join(sessions));
            } else if is_sessions_dir_name(child_name)
                && let Some(runtime) = paired_runtime_name(child_name)
            {
                // A marker dir with no runtime sibling. `acquire` mints it before
                // it builds the tree, so a crash in that window strands one — and
                // per-session keying makes that a fresh empty dir every time
                // rather than a reused one. A legacy `sessions/` holding only
                // upgrade-compat markers lands here too, and is spared until the
                // last of them is released.
                let runtime = profile.join(runtime);
                if runtime.symlink_metadata().is_err() {
                    let _ = gc_one_pair(&runtime, &child.path());
                }
            }
        }
    }
}

/// Drop the markers of bare `claude` sessions that have exited — the ordinary
/// case, since such a session never runs clauth code and leaves its file behind.
/// The same per-entry prune the paired trees get, minus the tree removal: this
/// dir holds nothing but markers, so no `remove_dir_all` is handed anything here.
fn gc_bare_markers() {
    let Ok(dir) = live_bare_dir() else {
        return;
    };
    // Peek before locking. This runs at every `clauth mcp` boot — including the
    // Plugin tab's probe child, which dies at 3s — while the state flock waits up
    // to `STATE_LOCK_TIMEOUT` and is legitimately held ~20s by a macOS switch's
    // keychain shell-out. Nothing to prune must not pay that wait. A marker that
    // appears after the peek is collected by the next sweep, which is the whole
    // contract of a best-effort GC; a marker that DISAPPEARS after it leaves
    // `prune_stale_sessions` a `NotFound` it already treats as nothing to do.
    let Ok(mut entries) = std::fs::read_dir(&dir) else {
        return;
    };
    if entries.next().is_none() {
        return;
    }
    let _ = with_state_lock(|_held| {
        let _ = prune_stale_sessions(&dir);
        Ok::<_, anyhow::Error>(())
    });
}

/// Drop registry rows whose owning session is gone. A row is dead iff no marker
/// the session could still hold is flock-held: the attributed member first (a
/// swapped session runs there), then the launch member. This keeps GC aligned
/// with [`crate::live_sessions::LiveTally::collect`] — a row can never be alive in
/// the tally and dead to GC, which is what would silently reap a live swapped row
/// after a force-delete of its launch profile. Folded in here rather than given
/// its own entry point so every existing `gc_stale_runtimes` caller gets it.
fn gc_live_session_rows() {
    for row in crate::live_sessions::list() {
        let probe = ProfileName::from(row.current_member.as_deref().unwrap_or(&row.start_profile));
        if !session_row_is_live(&probe, row.isolated, &row.session_id)
            && let Err(e) = crate::live_sessions::unregister(&row.session_id)
        {
            logline!("clauth: dropping stale live-session row failed: {e}");
        }
    }
}

/// Collect one paired (`runtime<rest>`, `sessions<rest>`) tree when nothing holds
/// a marker in it. The two go together; a `runtime` path that does not exist
/// collects the orphaned marker dir alone.
fn gc_one_pair(runtime: &Path, sessions: &Path) -> Result<()> {
    with_state_lock(|_held| {
        // An unknown reads as live: this leg runs from the daemon's timer, in a
        // different process, against every profile, and under `LinkMode::Fake`
        // the tree it would remove is the one a live sibling is running out of.
        if prune_stale_sessions(sessions).unwrap_or(1) == 0 {
            let _ = std::fs::remove_dir_all(runtime);
            let _ = std::fs::remove_dir(sessions);
        }
        Ok::<_, anyhow::Error>(())
    })
}

/// Every profile's SHARED runtime dirs: each live session's `runtime-<sid>` plus
/// a legacy bare `runtime` an earlier release left behind. Isolated dirs are
/// excluded — both config reconcilers walk this, and neither may reach an
/// isolated copy (why at [`crate::jsonsync::runtime_files_under`]). Fail-soft: an
/// unreadable root or profile contributes nothing. Runs on the ~10 Hz watchdog
/// tick, so it allocates only the paths it returns.
pub(crate) fn shared_runtime_dirs() -> Vec<PathBuf> {
    let Ok(root) = profiles_root_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for profile in entries.flatten() {
        if !profile.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(children) = std::fs::read_dir(profile.path()) else {
            continue;
        };
        for child in children.flatten() {
            if child
                .file_name()
                .to_str()
                .is_some_and(is_shared_runtime_dir_name)
            {
                out.push(child.path());
            }
        }
    }
    out
}

/// Every live isolated SESSION, paired with the `runtime-isolated…/projects/`
/// dir backing it — so under real symlinks a profile running two isolated
/// sessions appears twice, once per store. A consumer keying by profile name must
/// expect that. Under [`LinkMode::Fake`] both sessions share one store, so the
/// profile appears once; a consumer must not read the row count as a session
/// count either way.
///
/// An isolated runtime's transcripts live
/// ONLY in this throwaway tree (never symlinked to the global store) and are
/// discarded on teardown/GC, so the session index can reach them only while the
/// session is live. Gated on a live *isolated* session specifically (not
/// [`has_live_session`], which also counts shared sessions) and on the projects
/// dir existing, so a shared-only or not-yet-written runtime is skipped.
/// Fail-soft: an unreadable profiles root or entry is skipped, never an error.
#[allow(
    dead_code,
    reason = "consumed by the session index (src/sessions.rs), wired into a surface in a later phase"
)]
pub(crate) fn live_isolated_stores() -> Vec<(String, PathBuf)> {
    let Ok(root) = profiles_root_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for profile in entries.flatten() {
        let profile_name = profile.file_name();
        let Some(profile_name) = profile_name.to_str() else {
            continue;
        };
        let profile_path = profile.path();
        let Ok(children) = std::fs::read_dir(&profile_path) else {
            continue;
        };
        for child in children.flatten() {
            let file_name = child.file_name();
            let Some(child_name) = file_name.to_str() else {
                continue;
            };
            if !child_name.starts_with(ISOLATED_RUNTIME_STEM) {
                continue;
            }
            let Some(sessions) = paired_sessions_name(child_name) else {
                continue;
            };
            if live_sessions_at(&profile_path.join(sessions)).is_some_and(|n| n == 0) {
                continue;
            }
            let projects = child.path().join("projects");
            if projects.is_dir() {
                out.push((profile_name.to_string(), projects));
            }
        }
    }
    out
}

fn canonical_credentials(name: &ProfileName) -> Result<PathBuf> {
    // CLA-ROLL: arm a rolling-token profile's sidecar BEFORE resolving the source —
    // a session launched inside an arming window (flag on, sidecar not yet
    // rolling) would otherwise copy the rotating pair, and the daemon's later
    // rotations (exempted from the live-session bail only for ARMED
    // sidecars) could still race a hand-armed state. Best-effort by design.
    crate::claude::arm_rolling_from_disk(name);
    // CLA-SPLIT: a `clauth start` session runs on what a switch would install —
    // the static session token when the profile has one. The rotating usage
    // pair in `credentials.json` must never be handed to a session (it would
    // re-arm the session-vs-refresher single-use-chain race the split removes).
    crate::claude::install_source_path(name)
}

/// Where `name`'s rotation lock lives: `~/.clauth/rotation-locks/<name>.lock`,
/// deliberately OUTSIDE the profile directory.
///
/// Inside it, `actions::delete_profile`'s `remove_dir_all` unlinked the very
/// inode the deleting process was holding, and the next acquire — finding no
/// file at that path — was granted a second holder of the same profile's lock
/// and recreated the profile directory to put it in.
///
/// `validate_profile_name` bounds a name to ASCII alphanumerics plus `-_.@+`
/// with no leading dot, so `<name>.lock` is always one flat component; the same
/// bound `profiles/<name>` already relies on. `pub(crate)` so a test names this
/// path by calling it rather than rebuilding the spelling.
///
/// Nothing reaps these files: a deleted profile, and the old name of a renamed
/// one, each leave a zero-byte lock behind for good. Deliberate, and it is the
/// shape the bug above demands — the only code positioned to unlink one is the
/// delete, which would be unlinking a lock it is itself holding. A stale file is
/// inert (`open_state_file` is `O_CREAT` without truncate, so a later profile of
/// the same name locks the same inode and nothing carries over), and the ceiling
/// is one empty file per name ever used. Its cost is one `symlink_metadata` per
/// orphan on each unix `load_config`, which walks the whole tree through
/// `enforce_clauth_perms` — that is the syscall the code makes, read off it and
/// not timed, so treat the magnitude as unmeasured. If it ever matters, the
/// upgrade is a sweep over this directory keyed on names absent from state,
/// taking each lock before unlinking it — never a reap inside the delete, which
/// would unlink a lock its own caller is holding.
pub(crate) fn rotation_lock_path(name: &ProfileName) -> Result<PathBuf> {
    Ok(clauth_dir()?
        .join("rotation-locks")
        .join(format!("{name}.lock")))
}

/// Cross-process advisory lock serializing a token rotation against a
/// `clauth start` session acquire for the SAME profile.
///
/// A refresh token is single-use: once `oauth::refresh_result` spends it the server
/// kills it, and a second refresh of the same token returns `invalid_grant`,
/// costing the losing caller its token (not the account — the pair minted by
/// the first spend survives, measured).
/// The global state flock (`with_state_lock`) cannot guard this because
/// it must be released across the network round trip; the per-PID session
/// flocks only track liveness, not "a rotation is in flight". This lock is
/// held for the FULL rotate HTTP window (which `with_state_lock` cannot be),
/// and `ProfileRuntime::acquire` takes the same lock before it stamps its
/// session PID file — so the two operations are mutually exclusive:
///
/// - rotate wins the race → acquire waits until the new pair is persisted, then
///   the session starts against the rotated token — or, if the rotation outlasts
///   the wait's deadline, fails with the named refusal and no session starts;
/// - acquire wins the race → it creates its session PID file before releasing,
///   so a macOS rotate's in-lock [`rotation_blocked_by_live_session`] check sees
///   the live session and skips. Off macOS the rotate proceeds and the session
///   picks the new pair up on its next request, so what the lock buys there is
///   ordering alone: the two rotations serialize instead of double-spending.
///
/// Distinct from `~/.clauth/.lock` (global state) and a session's own marker
/// file (per-session liveness). [`RotationGuard::acquire`] blocks with no
/// deadline; [`RotationGuard::acquire_with_timeout`] is the bounded form a
/// session start takes.
#[must_use]
pub(crate) struct RotationGuard {
    // Drops before `_rank` (declaration order): the flock releases, then the
    // ROTATION rank pops — never the reverse.
    _file: File,
    _rank: crate::lockorder::RankGuard,
}

/// How long [`ProfileRuntime::acquire`] waits for this profile's rotation lock
/// before failing with a [`RotationLockTimeout`].
///
/// It is NOT the sum of every leg a holder can run, and must not be re-derived as
/// one. Four legs have no bound at all: every phase of the token call except the
/// connect — header receipt included, which reads bounded and is not
/// ([`crate::oauth::TOKEN_HTTP_DEADLINES`] carries the measurement) — a
/// sibling start's recursive `~/.claude` copy inside
/// [`build_runtime_dir_with_active_env`], the state-flock acquisitions of the
/// longest holder — `oauth::gate_under_guard`, which takes up to four and whose
/// count moves with its quarantine branches — and, on macOS, a state flock a peer
/// is legitimately holding across its own Keychain budget. A sum over legs like
/// those is a number wearing a proof's clothes.
///
/// So it is derived from the two ends that are fixed.
///
/// The FLOOR is the two legs a HEALTHY holder spends real time in, so an ordinary
/// rotation is waited out rather than refused:
/// - [`crate::oauth::TOKEN_HTTP_DEADLINES`] — every rotation makes exactly
///   one token call, and this is what the two deadlines that call carries add up
///   to. The floor wants a number a healthy call comfortably fits inside, which
///   this is; it is not a ceiling, and the constant's own doc says why.
/// - [`KEYCHAIN_MIRROR_BUDGET`] — a macOS rotation mirrors the new pair into the
///   Keychain and may spend that whole budget doing it. Never
///   `crate::lock::SUBPROCESS_BUDGET`, which the two coincide with today: that one
///   bounds a state-flock hold's shell-outs in aggregate and
///   `oauth::apply_rotated_tokens_locked` runs its mirror AFTER the closure ends,
///   where nothing clamps it. Kept in the sum on every host, for the reason stated
///   at the constant.
///
/// Everything else a healthy holder does is sub-millisecond disk work ON LINUX,
/// which is [`crate::lock::state_lock_timeout`]'s own qualification of the same
/// claim. A state-flock acquisition that reaches that deadline is the wedge THAT
/// constant exists to name, so it is what this deadline waits out rather than
/// something to budget for — but on macOS a peer can legitimately hold that flock
/// for most of its 25 s, and a start queued behind a rotation queued behind such a
/// peer is refused here. Accepted: it needs three-way concurrency plus a keychain
/// slow enough to burn its budget, which is an unanswered ACL dialog or a locked
/// keychain, and the refusal is retryable.
///
/// The CEILING is [`crate::mcp::MAX_WAIT_SECS_NO_PROGRESS`], the only deadline
/// this wait sits inside: the MCP `delegate`'s pre-spawn window emits no progress
/// notification, there being no child to report on yet, and that constant is the
/// crate's already-derived answer to how long a peer that cannot receive progress
/// tolerates silence — itself a conservative proxy for Claude Code's 30-minute
/// stdio idle abort, not that abort. Past the abort the named refusal below
/// reaches nobody. Pinned as a relation rather than restated here.
///
/// A holder past this deadline gets a named retry rather than a fault, because
/// the unbounded legs mean a firing is not proof of a wedge.
///
/// `saturating_add` over `as_secs()` arithmetic: both terms are whole seconds
/// today, and a sub-second one added later would round DOWN through `as_secs`,
/// quietly shortening the deadline it was meant to lengthen.
pub(crate) const ROTATION_LOCK_TIMEOUT: Duration =
    crate::oauth::TOKEN_HTTP_DEADLINES.saturating_add(KEYCHAIN_MIRROR_BUDGET);

/// What a macOS rotation's Keychain mirror may spend under the rotation lock: two
/// `security` invocations at `keychain::SECURITY_TIMEOUT` each, unclamped because
/// `oauth::apply_rotated_tokens_locked` runs the mirror after its state-flock
/// closure ends.
///
/// Spelled here rather than read out of `keychain`, which is macOS-gated while
/// this deadline is one number on every host. Off macOS the term is not waste: it
/// is the headroom the only slow leg a holder has there — the token call,
/// which the other term derives for — would otherwise have none of. `keychain` holds
/// the other side of the derivation as a `const` assertion, so a re-tune of
/// `SECURITY_TIMEOUT` fails to COMPILE on the platform that has one.
pub(crate) const KEYCHAIN_MIRROR_BUDGET: Duration = Duration::from_secs(20);

/// The rotation-lock deadline a session start waits out: [`ROTATION_LOCK_TIMEOUT`],
/// or a shorter value a test poses a wedge under. The one source of the deadline
/// so a test can shrink the whole wait without sleeping it out, and production
/// never sets the override — mirrors [`crate::lock::state_lock_timeout`].
pub(crate) fn rotation_lock_timeout() -> Duration {
    #[cfg(test)]
    if let Some(t) = ROTATION_LOCK_TIMEOUT_OVERRIDE.with(std::cell::Cell::get) {
        return t;
    }
    ROTATION_LOCK_TIMEOUT
}

// Test seam shortening `rotation_lock_timeout` so a wedge can be posed without a
// real multi-minute wait. `None` is the production deadline. Thread-local, so a
// test that shortens it only affects the thread it drives the acquire on.
#[cfg(test)]
thread_local! {
    static ROTATION_LOCK_TIMEOUT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Set or clear the test-only deadline override. `None` restores
/// [`ROTATION_LOCK_TIMEOUT`].
#[cfg(test)]
pub(crate) fn set_rotation_lock_timeout_override(timeout: Option<Duration>) {
    ROTATION_LOCK_TIMEOUT_OVERRIDE.with(|c| c.set(timeout));
}

/// The profile's rotation lock could not be taken within its deadline: another
/// clauth process is rotating this account's chain or starting a session on it.
///
/// A recoverable, retry-later condition kept as a distinct type (surfaced through
/// `anyhow`) so a caller can `downcast_ref` and retry rather than read it as a
/// fault — the same split [`crate::lock::StateLockTimeout`] draws one lock
/// further in, and the reason `Cause::RotationLockUnavailable`'s copy insists a
/// failed BLOCKING acquire is never contention: with a deadline in play the two
/// outcomes are finally distinguishable, and they get different types.
///
/// The copy names no PROCESS, where [`crate::lock::StateLockTimeout`]'s does:
/// that lock sits behind an in-process mutex (`THREAD_LOCK`), so reaching its
/// flock deadline really does mean a second process. This one has no such mutex,
/// and N same-profile `delegate` calls are N threads of one MCP server contending
/// on it directly.
#[derive(Debug)]
pub(crate) struct RotationLockTimeout {
    name: String,
    waited: Duration,
}

impl std::fmt::Display for RotationLockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "timed out after {:.0}s taking '{}' rotation lock; a token rotation or another \
             session start is holding it — retry, or start a different account",
            self.waited.as_secs_f64(),
            self.name,
        )
    }
}

impl std::error::Error for RotationLockTimeout {}

impl RotationGuard {
    /// Open (creating if absent) this profile's rotation lock file, unlocked.
    /// Shared by all three acquisitions so they cannot drift on where the file
    /// lives or how it is created; it makes no profile directory, so a caller
    /// that needs one makes it itself.
    fn open(name: &ProfileName) -> Result<(PathBuf, File)> {
        let path = rotation_lock_path(name)?;
        if let Some(parent) = path.parent() {
            crate::profile::mkdir_700(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file =
            open_pid_file(&path).with_context(|| format!("failed to open {}", path.display()))?;
        Ok((path, file))
    }

    /// ROTATION is the outermost rank — held across the OAuth HTTP round trip,
    /// before `config` and the state flock are ever taken. Entered only once the
    /// flock is actually held, so a failed acquisition leaves no rank behind.
    fn held(file: File) -> Self {
        let _rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::Rotation>();
        Self { _file: file, _rank }
    }

    /// Acquire the per-profile rotation lock, blocking until any in-flight
    /// rotation or acquire for this profile releases it. Creates the
    /// rotation-locks directory if missing.
    ///
    /// No deadline, deliberately: every caller left on this form would rather
    /// wait a rotation out than act around it, and their own docs rest on the
    /// blocking (`oauth::LockWait::Block`, `claude::arm_rolling_from_disk`).
    /// A session start is the one caller that cannot — see
    /// [`acquire_with_timeout`](Self::acquire_with_timeout).
    pub(crate) fn acquire(name: &ProfileName) -> Result<Self> {
        let (path, file) = Self::open(name)?;
        file.lock()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self::held(file))
    }

    /// [`acquire`](Self::acquire) with a deadline: a wait that reaches `timeout`
    /// fails with a [`RotationLockTimeout`] instead of parking forever.
    ///
    /// The session-start form. A start has a caller waiting on it — an operator
    /// at a spinner, or an MCP `delegate` whose pre-spawn window sends no
    /// progress notification at all — and an unbounded park there is
    /// indistinguishable from a hang, with nothing on the wire to say which. The
    /// deadline turns it into a named condition the caller can retry.
    ///
    /// The wait itself is still the BLOCKING `File::lock()`, moved onto a helper
    /// thread, rather than the `try_lock` poll `crate::lock` uses on the state
    /// flock. Polling would have been the smaller diff and is the wrong shape
    /// here: waiters do not randomize phase, so they all fail one `try_lock`
    /// together, all sleep, and one wins per wake — making every handoff cost a
    /// full poll interval and the queue cost `interval x position`, independent
    /// of how long the hold actually is. That floor is the thing this task exists
    /// to lower. A kernel wakeup keeps the queue exactly as fast as it is today,
    /// which is what makes "the wait is now bounded" cost nothing rather than
    /// something too small to have noticed.
    ///
    /// The helper is deliberately not joined. It resolves no path and reads no
    /// config — it holds an already-open fd and calls `lock()` — so it cannot
    /// reach a real `~/.clauth` after a test's home override clears, which is
    /// what `testutil::HomeSandbox`'s join is for. On the timeout path the send
    /// finds no receiver, the `File` drops with the `SendError`, and the flock
    /// releases the moment the wedge does.
    ///
    /// The cost of not joining, stated rather than left to be discovered: one
    /// parked thread and one open fd per TIMED-OUT acquisition, for the wedge's
    /// lifetime, with no cap. An uncontended acquire spawns nothing and a waited-out
    /// one drains at the handoff, so only a caller retrying against a wedge
    /// accumulates — a `clauth mcp` agent re-issuing `delegate` is the shape.
    /// Releasing the wedge drains every one of them promptly, and each drains by
    /// taking the flock for an instant, which a concurrent `try_acquire` reads as
    /// contention. The ceiling that matters is the process fd limit: at a 1024 soft
    /// `RLIMIT_NOFILE` it takes roughly a thousand timed-out retries, each waiting
    /// out [`ROTATION_LOCK_TIMEOUT`], to reach it.
    pub(crate) fn acquire_with_timeout(name: &ProfileName, timeout: Duration) -> Result<Self> {
        let (path, file) = Self::open(name)?;
        match file.try_lock() {
            Ok(()) => return Ok(Self::held(file)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
        let (tx, rx) = crossbeam_channel::bounded::<std::io::Result<File>>(1);
        let display = path.display().to_string();
        thread::Builder::new()
            .name(format!("clauth-rotwait-{name}"))
            .spawn(move || {
                let taken = file.lock();
                let _ = tx.send(taken.map(|()| file));
            })
            .with_context(|| format!("failed to spawn the wait for {display}"))?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(file)) => Ok(Self::held(file)),
            Ok(Err(e)) => Err(e).with_context(|| format!("failed to lock {display}")),
            // Disconnected can only mean the helper panicked before sending;
            // treating it as the deadline would claim a wait that never happened.
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                anyhow::bail!(
                    "waiting for {display} ended with no verdict; the thread holding the \
                     wait died — retry the command"
                )
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let timed_out = RotationLockTimeout {
                    name: name.to_string(),
                    waited: timeout,
                };
                // Carries the lock PATH, which the error deliberately does not:
                // off a TTY the logline and the error both land on stderr, and a
                // verbatim copy of the sentence the caller is about to render
                // helps nobody. What a wedge diagnosis wants is the file.
                logline!("clauth: {timed_out} ({display})");
                Err(anyhow::Error::new(timed_out))
            }
        }
    }

    /// Like [`RotationGuard::acquire`], but `Ok(None)` when another holder has
    /// the lock instead of parking behind it. For callers on threads that must
    /// never wait at all — the scheduler's tick thread above all, where a
    /// `clauth start` holding this lock across its recursive `~/.claude` copy
    /// would otherwise stall every account's poll while the heartbeat (stamped
    /// in the main loop, not here) stays fresh.
    pub(crate) fn try_acquire(name: &ProfileName) -> Result<Option<Self>> {
        let (path, file) = Self::open(name)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
        Ok(Some(Self::held(file)))
    }
}

/// Open or create a PID file without truncating — used for session liveness
/// tracking via flock. `O_CREAT` without truncate preserves any existing lock
/// held by a sibling that raced us to create the file. Owner-only (0o600) via
/// [`crate::profile::open_state_file`], the shared opener for every `~/.clauth`
/// lock (this also covers the rotation lock at [`rotation_lock_path`], opened
/// through here).
pub(crate) fn open_pid_file(path: &Path) -> std::io::Result<File> {
    crate::profile::open_state_file(path)
}

/// Why this host cannot execute a per-session credential swap at all. Both arms
/// are structural rather than unfinished work, and both must REFUSE loudly: a
/// swap that silently leaves the session on its launch account is the one outcome
/// the live-Claude-Code probe exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapUnsupported {
    /// [`LinkMode::Fake`] shares ONE runtime tree across every session of the
    /// profile+flavor, so repointing its credential file would move every session
    /// of that profile at once.
    SharedRuntimeTree,
    /// macOS resolves credentials Keychain-FIRST and deletes the plaintext file
    /// once it has migrated them, so a swapped-in file is inert until the
    /// per-`CLAUDE_CONFIG_DIR` Keychain item is written alongside it.
    KeychainFirst,
}

impl std::fmt::Display for SwapUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SharedRuntimeTree => {
                f.write_str("this host shares one runtime tree across the profile's sessions")
            }
            Self::KeychainFirst => f.write_str("this host resolves credentials keychain-first"),
        }
    }
}

/// The transport/platform gate, kept PURE so both refusals are exercised from a
/// Linux test run — reading `cfg!(target_os = "macos")` is the caller's job.
fn swap_support(mode: LinkMode, is_macos: bool) -> Result<(), SwapUnsupported> {
    if mode == LinkMode::Fake {
        return Err(SwapUnsupported::SharedRuntimeTree);
    }
    if is_macos {
        return Err(SwapUnsupported::KeychainFirst);
    }
    Ok(())
}

/// Whether a requested `--with-fallback` opt-in survives to the registry row.
///
/// The row must never claim a session follows the chain where the executor
/// structurally refuses ([`swap_support`], [`SwapRefused::IsolatedSession`]): a
/// daemon tick landing on such a row writes an intent nothing can execute, and
/// the executor's refusal dedupe is per `(member, reason)`, so it says so exactly
/// once into a log nobody is reading. The transport mode is known only inside
/// [`ProfileRuntime::acquire`]'s state-lock hold — the same hold that writes the
/// row — so this is where the two are kept consistent. The USER-facing refusal is
/// `start::run`'s, before a tree is built or `claude` is spawned; this is the
/// floor under it, not a substitute for it.
fn chain_opt_in_survives(
    requested: bool,
    isolation: Isolation,
    mode: LinkMode,
    is_macos: bool,
) -> bool {
    requested && isolation == Isolation::Shared && swap_support(mode, is_macos).is_ok()
}

/// [`swap_support`]'s PLATFORM arm alone, answerable with no disk: the mode is
/// pinned to the supported transport, so only `is_macos` can fire. `start::run`
/// asks this before anything fallible, because a verdict fixed at compile time
/// must not reach the user as a state-lock timeout or an IO error from a probe it
/// never needed.
///
/// Deliberate consequence: on a macOS host that ALSO runs [`LinkMode::Fake`] the
/// user hears the keychain cause rather than the shared-tree one, inverting
/// `swap_support`'s own arm precedence. Both arms are unfixable dead ends and the
/// executor's arm order is untouched, so this only picks which of the two is named.
pub(crate) fn unsupported_swap_platform(is_macos: bool) -> Option<SwapUnsupported> {
    swap_support(LinkMode::Real, is_macos).err()
}

/// [`swap_support`]'s TRANSPORT arm, which is only knowable by probing. Run LAST
/// among the `--with-fallback` gates: it is the one leg that writes, so a start
/// refused for any other cause never materializes a profile dir for an account
/// that never launched.
///
/// Probes the profile dir exactly as [`ProfileRuntime::acquire`] does, under the
/// same state lock, so two concurrent starts cannot interleave their probe
/// dotfiles and read a spurious [`LinkMode::Fake`]. `is_macos` is pinned false
/// because [`unsupported_swap_platform`] already owns that arm.
///
/// `Ok(None)` is the supported host. An IO failure propagates rather than reading
/// as either answer — a probe that could not run says nothing about the host.
pub(crate) fn unsupported_swap_transport(name: &ProfileName) -> Result<Option<SwapUnsupported>> {
    let profile_root = profile_dir(name)?;
    let mode = with_state_lock(|_held| {
        crate::profile::mkdir_700(&profile_root)
            .with_context(|| format!("failed to create {}", profile_root.display()))?;
        detect_link_mode(&profile_root)
    })?;
    Ok(swap_support(mode, false).err())
}

/// Why a swap onto a named member did not happen. A VALUE rather than an error:
/// each arm is a decision the executor takes deliberately, and each is logged,
/// because a silent refusal leaves the session authenticating as its launch
/// account with nothing reporting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwapRefused {
    Unsupported(SwapUnsupported),
    /// Teardown has begun. `Drop` joins the watchdog, so a swap started now would
    /// hold session exit for the state-lock timeout plus an unbounded
    /// rotation-flock wait.
    ShuttingDown,
    /// The link already resolves to this member.
    AlreadyCurrent,
    /// An `--isolated` session: a throwaway tree, deliberately not part of any
    /// chain.
    IsolatedSession,
    ProfileUnreadable(String),
    /// Carries a `base_url`: a different endpoint, not the same account elsewhere.
    NotOauth,
    Disabled,
    /// `settings.json` env reaches Claude Code's `process.env` only at STARTUP,
    /// so a member with different env is a genuinely different transport.
    EnvDiffers,
    ModelsDiffers,
    ApiKeyDiffers,
    /// Nothing at `install_source_path` — there is no login to swap onto.
    NoCredentialStore,
    /// A live process holds the marker this session would need on the intended
    /// member — a colliding session id. Claiming it anyway would leave two
    /// sessions sharing one marker identity, and teardown unlinks only what it
    /// owns, so the survivor would be reported dead while it runs.
    MarkerNotLockable,
}

impl std::fmt::Display for SwapRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(why) => write!(f, "{why}"),
            Self::ShuttingDown => f.write_str("the session is shutting down"),
            Self::AlreadyCurrent => f.write_str("the link already resolves to it"),
            Self::IsolatedSession => f.write_str("an isolated session follows no chain"),
            Self::ProfileUnreadable(e) => write!(f, "its profile could not be read: {e}"),
            Self::NotOauth => f.write_str("it carries a custom endpoint"),
            Self::Disabled => f.write_str("it is disabled"),
            Self::EnvDiffers => f.write_str("its custom env differs from the launch snapshot"),
            Self::ModelsDiffers => {
                f.write_str("its model routing differs from the launch snapshot")
            }
            Self::ApiKeyDiffers => {
                f.write_str("its api-key state differs from the launch snapshot")
            }
            Self::NoCredentialStore => f.write_str("it has no stored login"),
            Self::MarkerNotLockable => {
                f.write_str("its liveness marker is held by another process")
            }
        }
    }
}

/// What one swap attempt did. Genuine IO failures propagate as an
/// [`anyhow::Error`]; a refusal is a value, since it is a decision rather than a
/// fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwapOutcome {
    Swapped,
    Refused(SwapRefused),
}

/// The transport a session's Claude Code actually booted with. Compared against
/// the INTENDED member rather than against a re-read of the current one, because
/// this snapshot is what is live in the child's `process.env`: `settings.json`
/// env is applied at startup only.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LaunchTransport {
    env: std::collections::BTreeMap<String, String>,
    models: crate::profile::ModelSettings,
    has_api_key: bool,
}

impl LaunchTransport {
    pub(crate) fn of(profile: &Profile) -> Self {
        Self {
            env: profile.env.clone(),
            models: profile.models.clone(),
            has_api_key: profile.api_key.is_some(),
        }
    }
}

/// Whether `candidate` is a member this session could be swapped onto, judged on
/// CONFIG grounds alone — no disk IO, no platform or transport-mode check.
///
/// Shared with the daemon's per-session decision leg, which needs it as a walk
/// PREFERENCE: a candidate the executor refuses is one the walk must step PAST,
/// or the intent stops changing and the session never reaches the next viable
/// member (a chain holding a `z.ai` or DeepSeek profile is enough to do it). One
/// function rather than two so the preference and the gate cannot drift.
///
/// The executor stays the safety gate. What is deliberately NOT here: the
/// store-exists check (disk IO the daemon would repeat per candidate per tick)
/// and the transport/platform + isolated refusals ([`swap_support`], which the
/// decision leg has no business re-deriving).
pub(crate) fn swap_eligible(
    candidate: &Profile,
    launch: &LaunchTransport,
) -> Result<(), SwapRefused> {
    if !candidate.is_oauth() {
        return Err(SwapRefused::NotOauth);
    }
    if candidate.is_disabled() {
        return Err(SwapRefused::Disabled);
    }
    if candidate.env != launch.env {
        return Err(SwapRefused::EnvDiffers);
    }
    if candidate.models != launch.models {
        return Err(SwapRefused::ModelsDiffers);
    }
    if candidate.api_key.is_some() != launch.has_api_key {
        return Err(SwapRefused::ApiKeyDiffers);
    }
    Ok(())
}

/// A precondition-cleared swap target, minted ONLY by
/// [`SessionSwap::precondition`] — which gets here by way of
/// `profile::load_profile`, and THAT load is what adopts-or-discards a
/// `credentials.json.pending` sidecar and then removes it.
///
/// [`touch_store`] takes one of these as its argument for exactly that reason:
/// the stamp it leaves is only readable as a non-write for as long as its receipt
/// stands, so the sidecar is still resolved against real bytes first. There is no
/// other constructor, so the touch cannot be reached without the load.
struct SwapPlan {
    member: ProfileName,
    store: PathBuf,
}

/// Move the mtime of the store the credential link is about to resolve to.
///
/// Claude Code stats the symlink's TARGET at the head of every request and clears
/// its process-wide token memo when that value is not EQUAL to the one it
/// memoized for the target it last stat'd — `if(e!==Oeu)`, an inequality, not an
/// ordering. So the whole job is to make the new store's mtime differ from
/// `memoized`; an mtime-preserving repoint is a silent no-op, the session keeps
/// authenticating as the old member, and nothing anywhere reports a problem.
///
/// Runs BEFORE the repoint, so a failure here leaves nothing moved.
///
/// The bump carries no write behind it, which is exactly what
/// `profile::recover_pending_credentials` and [`resolve_credential_winner`] would
/// read it as. A [`crate::profile_cache::TouchReceipt`] beside the store is what
/// lets them tell the two apart; both resolve their store mtime through
/// [`crate::profile_cache::effective_write_time`].
fn touch_store(plan: &SwapPlan, memoized: Option<SystemTime>) -> Result<()> {
    // Through the resolver, not the raw mtime: on a swap BACK onto a member the
    // value being displaced is that member's own earlier stamp, and a receipt
    // recording a stamp as a write time hands the readers the exact answer this
    // exists to prevent — eroding by one stamp per revisit, on the chain churn
    // the feature is for.
    let displaced = crate::profile_cache::effective_write_time(&plan.store);
    let file = OpenOptions::new()
        .write(true)
        .open(&plan.store)
        .with_context(|| format!("failed to open {}", plan.store.display()))?;
    let stamp = |at: SystemTime| {
        file.set_times(std::fs::FileTimes::new().set_modified(at))
            .with_context(|| format!("failed to touch {}", plan.store.display()))
    };
    let landed = || {
        file.metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(as_stored)
    };
    let asked = SystemTime::now();
    stamp(asked)?;
    // A receipt is only sound where the filesystem kept the EXACT value asked
    // for. Where the mtime truncates, a genuine write landing in the same tick
    // aliases onto the stamp, and a receipt resolving a real write back to
    // `displaced` inverts both decisions — worse than no receipt at all. Read
    // from the first stamp only: `memoized + 1s` below is already tick-aligned,
    // so it round-trips exactly even on the filesystem this guards against.
    let exact = landed() == Some(asked);
    // The clock is normally enough, because only EQUALITY hides the swap. The one
    // way `now` lands on `memoized` is a coarse-granularity filesystem truncating
    // it back onto a store written in the same second, so check what actually
    // landed rather than predict it. Stamping ahead of the clock stays the
    // fallback rather than the default: a store left in the future outlives any
    // receipt the moment something writes it.
    if let Some(memoized) = memoized
        && landed() == Some(memoized)
    {
        stamp(memoized + Duration::from_secs(1))?;
    }
    if exact && let Some(stamped) = landed() {
        crate::profile_cache::write_touch_receipt(&plan.member, &plan.store, stamped, displaced);
    }
    Ok(())
}

/// Test-only mtime-granularity override for [`touch_store`]'s read-back. Every
/// filesystem a Linux/macOS test run can reach (ext4, tmpfs, apfs) stores the
/// exact value `set_times` asked for, so the branch that withholds a receipt
/// where the mtime TRUNCATES has no other way to be exercised — and it is the
/// branch that fails silently, since a receipt issued on a truncating filesystem
/// aliases onto any write landing in the same tick. Serialized by
/// `profile::HOME_TEST_LOCK`, which every test that sets it already holds via
/// `with_fake_home`. Never compiled into the binary.
#[cfg(test)]
static COARSE_MTIME_OVERRIDE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// Gated with its caller: the one test that poses a truncating filesystem drives
// `swap_to`, refused at platform level on macOS, so an ungated setter there is a
// dead-code error under clippy `-D warnings`. The static stays ungated —
// `as_stored` reads it on every platform.
#[cfg(all(test, not(target_os = "macos")))]
fn set_coarse_mtime_override(on: bool) {
    COARSE_MTIME_OVERRIDE.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// What the filesystem would have stored for `at`. Identity everywhere except a
/// test that has asked to stand in for a one-second-granularity filesystem.
fn as_stored(at: SystemTime) -> SystemTime {
    #[cfg(test)]
    if COARSE_MTIME_OVERRIDE.load(std::sync::atomic::Ordering::SeqCst)
        && let Ok(since_epoch) = at.duration_since(std::time::UNIX_EPOCH)
    {
        return std::time::UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs());
    }
    at
}

/// A file's mtime, or `None` when it has none to read.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// One swapped-onto member's liveness markers, held for the session's life.
struct SwappedMarkers {
    pid_file: PathBuf,
    pid_lock: File,
    /// The upgrade-compat marker, `None` exactly as in [`ProfileRuntime`]: there
    /// is no second path to stamp. Its lock is `None` when [`stamp_legacy_marker`]
    /// lost `try_lock` to a live process holding a colliding sid, which is what
    /// keeps teardown from unlinking a file this session never owned.
    legacy_marker: Option<PathBuf>,
    legacy_lock: Option<File>,
}

/// What claiming a member's liveness markers found.
enum MarkerClaim {
    /// Freshly stamped; hold for the session's life.
    Stamped(SwappedMarkers),
    /// This session already holds them — a member it has run on before. `flock`
    /// locks the open file description, so a second `open` + `try_lock` from THIS
    /// process is denied by our own lock; without recognizing that, every swap back
    /// onto a recovered member would read as a foreign holder and be refused,
    /// which removes the chain's whole recovery half.
    AlreadyOurs,
    /// A live process that is not this one holds it.
    Foreign,
}

/// Stamp and hold the intended member's liveness markers, BOTH layouts.
///
/// The per-session one makes the swapped-onto member read live to
/// [`has_live_session`], which is what stops a delete or disable landing on the
/// account this session is now running as. It is NOT a rotation gate on this
/// platform: rotation refuses only on macOS
/// ([`rotation_blocked_by_live_session`]), and [`swap_support`] refuses to swap
/// there at all, so no swapped session ever meets that refusal.
///
/// The compat one is what a clauth predating the per-session layout reads, and
/// right after an upgrade that old binary is the running daemon. That binary DOES
/// still gate rotation on liveness, so missing it costs the live child one failed
/// refresh until the daemon is replaced.
///
/// `None` when the per-session marker is held by something else, so the caller
/// refuses the swap. A foreign holder means a colliding session id, and marker
/// ownership is what teardown keys on — unlinking a marker another session minted
/// would report that session dead while it runs. Callers go through
/// [`SessionSwap::claim_markers`], which separates a foreign holder from this
/// session's own earlier claim first.
fn stamp_swapped_markers(paths: &SessionPaths) -> Result<Option<SwappedMarkers>> {
    crate::profile::mkdir_700(&paths.sessions)
        .with_context(|| format!("failed to create {}", paths.sessions.display()))?;
    let pid_lock = open_pid_file(&paths.pid_file)
        .with_context(|| format!("failed to open {}", paths.pid_file.display()))?;
    // `try_lock`, not `lock`: this runs inside the state flock, where a blocking
    // wait on a foreign holder would park the watchdog thread.
    if pid_lock.try_lock().is_err() {
        return Ok(None);
    }
    let legacy_lock = paths.legacy_marker.as_deref().and_then(stamp_legacy_marker);
    Ok(Some(SwappedMarkers {
        pid_file: paths.pid_file.clone(),
        pid_lock,
        legacy_marker: paths.legacy_marker.clone(),
        legacy_lock,
    }))
}

/// The member a live session's credential link resolves to, and the markers of
/// every member it has run on.
struct SwapCell {
    member: String,
    canonical: PathBuf,
    /// Markers stamped by swaps, in visit order. NEVER released mid-session: the
    /// live Claude Code child still holds every refresh token it has been handed
    /// and nothing can observe when it stops using one.
    held: Vec<SwappedMarkers>,
    /// The last refusal announced, so a stuck `intended_member` states its reason
    /// once instead of once per watchdog tick.
    last_refusal: Option<(String, SwapRefused)>,
}

/// One-shot shutdown gate for a session's swap executor: `begin` is a Release
/// store and `is_begun` an Acquire load, so once teardown begins no later
/// precondition can pass — a swap is never STARTED mid-teardown, whichever of
/// the watchdog thread and `Drop` gets there first.
struct ShutdownFlag {
    inner: std::sync::atomic::AtomicBool,
}

impl ShutdownFlag {
    fn new() -> Self {
        Self {
            inner: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn begin(&self) {
        self.inner.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_begun(&self) -> bool {
        self.inner.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// The per-session credential swap executor: a live `clauth start` session moving
/// from the account it launched on to another chain member, without a restart and
/// without letting a rotation spend a single-use refresh token the live Claude
/// Code child still holds.
///
/// Shared between that session's watchdog thread — which executes swaps and reads
/// the current member on its credential leg — and its [`ProfileRuntime`], whose
/// final tick reads the same cell and whose teardown releases what a swap
/// stamped. A plain field could not serve either: both hold a MOVED CLONE of the
/// canonical path, so mutating one is invisible to the other and the next tick
/// would relink the session back onto its launch member AND write the new
/// member's tokens into the old member's store.
pub(crate) struct SessionSwap {
    session: SessionId,
    isolation: Isolation,
    mode: LinkMode,
    /// This session's own `CLAUDE_CONFIG_DIR`; its `.credentials.json` is what a
    /// swap repoints.
    runtime: PathBuf,
    launch: LaunchTransport,
    /// The launch member's per-session marker path. `ProfileRuntime` owns that fd
    /// for the session's life; the PATH lives here so a swap back onto the launch
    /// member recognizes the marker as this session's own rather than as a foreign
    /// holder — see [`MarkerClaim::AlreadyOurs`].
    launch_marker: PathBuf,
    cell: crate::lockorder::RankedMutex<SwapCell, crate::lockorder::rank::SwapCell>,
    shutdown: ShutdownFlag,
}

impl SessionSwap {
    /// `paths` is the LAUNCH member's resolved paths, so the runtime dir and the
    /// marker this session already holds come from one source and cannot disagree.
    fn new(
        session: SessionId,
        isolation: Isolation,
        mode: LinkMode,
        launch: &Profile,
        canonical: PathBuf,
        paths: &SessionPaths,
    ) -> Self {
        Self {
            session,
            isolation,
            mode,
            runtime: paths.runtime.clone(),
            launch: LaunchTransport::of(launch),
            launch_marker: paths.pid_file.clone(),
            cell: crate::lockorder::RankedMutex::new(SwapCell {
                member: launch.name.as_str().to_string(),
                canonical,
                held: Vec::new(),
                last_refusal: None,
            }),
            shutdown: ShutdownFlag::new(),
        }
    }

    fn cell(&self) -> crate::lockorder::RankedGuard<'_, SwapCell> {
        self.cell.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The member this session's credential link resolves to.
    fn member(&self) -> String {
        self.cell().member.clone()
    }

    /// The credential store the link resolves to. The watchdog's credential leg
    /// and `Drop`'s final tick both reach it through here, inside the same
    /// `with_state_lock` hold a swap publishes under.
    fn canonical(&self) -> PathBuf {
        self.cell().canonical.clone()
    }

    /// Claim `paths`'s markers for this session, separating a marker this session
    /// already holds from one a foreign process does. The two are
    /// indistinguishable to `try_lock`, and conflating them is what would refuse
    /// every swap back onto a member the session has already run on.
    fn claim_markers(&self, paths: &SessionPaths) -> Result<MarkerClaim> {
        let ours = paths.pid_file == self.launch_marker
            || self
                .cell()
                .held
                .iter()
                .any(|held| held.pid_file == paths.pid_file);
        if ours {
            return Ok(MarkerClaim::AlreadyOurs);
        }
        Ok(match stamp_swapped_markers(paths)? {
            Some(markers) => MarkerClaim::Stamped(markers),
            None => MarkerClaim::Foreign,
        })
    }

    /// Whether this refusal is news. The trigger re-fires every tick, so
    /// announcing unconditionally writes one line per second for as long as the
    /// daemon's intent stands — while a refusal nothing ever says leaves the
    /// session on its launch account invisibly. Records what it returns `true` for.
    fn should_announce(&self, intended: &str, why: &SwapRefused) -> bool {
        let mut cell = self.cell();
        if cell
            .last_refusal
            .as_ref()
            .is_some_and(|(member, seen)| member == intended && seen == why)
        {
            return false;
        }
        cell.last_refusal = Some((intended.to_string(), why.clone()));
        true
    }

    /// The swap leg of this session's own watchdog tick: execute a move when the
    /// daemon has named a member that differs from the one the link resolves to.
    /// The daemon writes `intended_member` only for a row whose `follows_chain` is
    /// set, which `clauth start --with-fallback` is the only thing that requests, so
    /// a plain `start` session polls and finds nothing to do.
    fn poll(&self) {
        let Some(intended) = crate::live_sessions::get(self.session.as_str())
            .and_then(|row| row.intended_member)
            .filter(|intended| *intended != self.member())
        else {
            return;
        };
        let sid = self.session.as_str();
        match self.swap_to(&intended) {
            Ok(SwapOutcome::Swapped) => {
                logline!("clauth: session {sid} swapped onto {intended}");
            }
            Ok(SwapOutcome::Refused(why)) => self.announce_refusal(&intended, why),
            Err(e) => logline!("clauth: session {sid} could not swap onto {intended}: {e:#}"),
        }
    }

    /// Log a refusal once per (member, reason) pair. The trigger re-fires every
    /// tick, so announcing unconditionally would write one line per second for as
    /// long as the daemon's intent stands — but a refusal that says nothing at all
    /// leaves the session on its launch account invisibly.
    fn announce_refusal(&self, intended: &str, why: SwapRefused) {
        if self.should_announce(intended, &why) {
            logline!(
                "clauth: session {} stays on {}: {intended} is not swappable ({why})",
                self.session.as_str(),
                self.member()
            );
        }
    }

    /// Every arm refuses distinctly, so the log names the cause. The [`SwapPlan`]
    /// it returns is the touch step's only key: `load_profile` below is what
    /// clears a crash-staged credential sidecar, and moving the store's mtime
    /// before that clearing would discard the sidecar for good.
    fn precondition(&self, intended: &str) -> Result<SwapPlan, SwapRefused> {
        let intended = ProfileName::from(intended);
        if self.shutdown.is_begun() {
            return Err(SwapRefused::ShuttingDown);
        }
        swap_support(self.mode, cfg!(target_os = "macos")).map_err(SwapRefused::Unsupported)?;
        // `--isolated` and fallback-following are mutually exclusive (a throwaway
        // tree versus swappable managed credentials). Enforced at the executor
        // because it is the one chokepoint every caller goes through, rather than
        // re-remembered by the decision leg and the flag separately.
        if self.isolation == Isolation::Isolated {
            return Err(SwapRefused::IsolatedSession);
        }
        if intended == self.member() {
            return Err(SwapRefused::AlreadyCurrent);
        }
        let profile = crate::profile::load_profile(&intended)
            .map_err(|e| SwapRefused::ProfileUnreadable(format!("{e:#}")))?;
        swap_eligible(&profile, &self.launch)?;
        let store = crate::claude::install_source_path(&intended)
            .map_err(|e| SwapRefused::ProfileUnreadable(format!("{e:#}")))?;
        if !store.exists() {
            return Err(SwapRefused::NoCredentialStore);
        }
        Ok(SwapPlan {
            member: intended,
            store,
        })
    }

    /// Publish the swap into the cell: the member and canonical store the cell
    /// names become what the credential leg and teardown read, a freshly stamped
    /// marker joins `held`, and the refusal memo clears.
    ///
    /// Called only after the link has moved, inside the same `with_state_lock`
    /// hold that spans the drain, the stamp and the repoint — the rank assert at
    /// the entry is that contract executable. A cell naming a member the link
    /// never reached would be permanent: `poll` filters on `member()` equality,
    /// so nothing retries, and the next tick would treat an interactive `/login`
    /// belonging to one member as the other's, writing it over a chain the
    /// session never authenticated as.
    ///
    /// Every member the session has run on keeps its markers for the session's
    /// life, so a claim never replaces one; the whole `held` vec is released by
    /// [`release_swapped_markers`](Self::release_swapped_markers) at teardown.
    fn publish_swap(&self, plan: &SwapPlan, claim: MarkerClaim) {
        debug_assert!(
            crate::lockorder::holds::<crate::lockorder::rank::State>(),
            "the swap cell is published only inside the state-flock hold, or a \
             marker saying the new member while the link still resolves to the \
             old one lets a rotation burn the old member's chain under the live \
             session"
        );
        {
            let mut cell = self.cell();
            cell.member = plan.member.to_string();
            cell.canonical = plan.store.clone();
            if let MarkerClaim::Stamped(markers) = claim {
                cell.held.push(markers);
            }
            cell.last_refusal = None;
        }
        debug_assert!(
            is_session_alive(&self.launch_marker),
            "the launch member's marker must still be held after publishing a swap — \
             marker lifetime must not be shortened, or collect() probing current_member \
             would read an alive session as dead"
        );
    }

    /// Move this session onto `intended`.
    ///
    /// ONE rotation guard: `RankGuard::enter` asserts a strictly greater rank and
    /// `Rotation` is the outermost, so a second guard panics in debug and in
    /// release degrades into a genuine ABBA deadlock on flocks that have no
    /// deadline. ONE state-lock hold spans the drain, the stamp, the repoint and
    /// the publish: a marker saying the new member while the link still resolves to
    /// the old one, for even a single watchdog tick, lets a rotation burn the old
    /// member's chain under the live session.
    ///
    /// Inside that hold the order is chosen so every failure lands on one side or
    /// the other and never between them. Everything that can fail runs BEFORE the
    /// link moves; [`publish_swap`](Self::publish_swap) runs only once it has.
    fn swap_to(&self, intended: &str) -> Result<SwapOutcome> {
        let plan = match self.precondition(intended) {
            Ok(plan) => plan,
            Err(refused) => return Ok(SwapOutcome::Refused(refused)),
        };
        let _rotation = RotationGuard::acquire(&plan.member)?;
        let link = self.runtime.join(".credentials.json");
        with_state_lock(|_held| {
            let current = self.canonical();
            // DRAIN. A Claude Code re-login sitting in the runtime file belongs to
            // the member the link STILL resolves to; once canonical moves, the
            // next tick would write those bytes into the new member's store and
            // its refresh token would be gone.
            sync_credentials_unlocked(&link, &current)?;

            let paths =
                SessionPaths::resolve(&plan.member, self.isolation, &self.session, self.mode)?;
            let claim = self.claim_markers(&paths)?;
            if matches!(claim, MarkerClaim::Foreign) {
                return Ok(SwapOutcome::Refused(SwapRefused::MarkerNotLockable));
            }
            // Re-checked in the hold, where it means something: both paths that
            // remove a stored login (`clear_profile_credentials`, `delete_profile`)
            // do the removal inside their own `with_state_lock`, so this cannot go
            // stale while we hold it. Without the re-check, `relink_to_canonical`
            // takes its store-is-gone branch and UNLINKS the live session's
            // credential file.
            if !plan.store.exists() {
                return Ok(SwapOutcome::Refused(SwapRefused::NoCredentialStore));
            }
            touch_store(&plan, file_mtime(&current))?;
            relink_to_canonical(&link, &plan.store)?;

            // Past here the session IS on the new member, so nothing may report
            // otherwise. Publish, then let a registry failure be logged rather
            // than propagated as a swap that did not happen — the same line
            // `acquire` takes for `register`, and for the same reason.
            self.publish_swap(&plan, claim);
            // A freshly loaded row, edited through the session's own field view:
            // a row read before the swap and stored after would revert an
            // `intended_member` the daemon wrote in between.
            if let Err(e) =
                crate::live_sessions::update_as_session(self.session.as_str(), |fields| {
                    fields.set_current_member(plan.member.as_str());
                    fields.set_last_swap_at(crate::usage::now_ms());
                })
            {
                logline!(
                    "clauth: session {} swapped onto {} but its row did not update: {e:#}",
                    self.session.as_str(),
                    plan.member
                );
            }
            Ok(SwapOutcome::Swapped)
        })
    }

    /// Release and unlink everything the swaps stamped. Called from `Drop`'s
    /// single teardown hold, and the mirror of the launch member's own leg: only a
    /// marker whose flock this session holds is unlinked, and a dir shared with
    /// other sessions goes only once the last of them has left.
    fn release_swapped_markers(&self) {
        // Taken out from under the cell first: the IO below acquires nothing, and
        // the rank is a true leaf only while it stays that way.
        let held = std::mem::take(&mut self.cell().held);
        for markers in held {
            let SwappedMarkers {
                pid_file,
                pid_lock,
                legacy_marker,
                legacy_lock,
            } = markers;
            // Release before unlinking, so a sibling's `prune_stale_sessions`
            // never reads a removed path.
            drop(pid_lock);
            if let Err(e) = std::fs::remove_file(&pid_file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                logline!(
                    "clauth: remove swapped marker {} failed: {e}",
                    pid_file.display()
                );
            }
            if let Some(legacy_marker) = legacy_marker {
                // `None` is a marker a live foreign process holds — unlinking it
                // would delete THEIR liveness signal.
                if legacy_lock.is_some() {
                    drop(legacy_lock);
                    let _ = std::fs::remove_file(&legacy_marker);
                }
                if let Some(dir) = legacy_marker.parent()
                    && prune_stale_sessions(dir).unwrap_or(1) == 0
                {
                    let _ = std::fs::remove_dir(dir);
                }
            }
            if let Some(dir) = pid_file.parent()
                && prune_stale_sessions(dir).unwrap_or(1) == 0
            {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

/// Live-session guard. On drop: stops the watchdog, runs a final sync
/// (errors surface to stderr), drops the PID file, and discards this session's
/// own runtime tree.
pub(crate) struct ProfileRuntime {
    /// Shared with the watchdog thread: the member the credential link resolves
    /// to, which a swap moves, plus everything that swap needs.
    swap: std::sync::Arc<SessionSwap>,
    pid_file: PathBuf,
    /// Upgrade-compat marker path, `None` when this session's own `pid_file`
    /// already sits there and there is nothing separate to stamp. See
    /// [`stamp_legacy_marker`] for its lifetime and the release at which both it
    /// and this field go away.
    legacy_marker: Option<PathBuf>,
    claude_home: PathBuf,
    sessions: PathBuf,
    /// Held for the lifetime of the session so a sibling process's
    /// `try_lock` reveals we're still alive.
    _pid_lock: File,
    /// The same signal at the pre-per-session path, for a still-running clauth
    /// that predates this layout. `None` when it could not be stamped.
    legacy_lock: Option<File>,
    /// Wrapped in Option so Drop can take() it before joining the watchdog,
    /// signalling the thread to exit.
    watchdog_signal: Option<crossbeam_channel::Sender<()>>,
    watchdog_handle: Option<JoinHandle<()>>,
}

/// Refuse a session for a name the on-disk record no longer carries.
///
/// Every caller hands [`ProfileRuntime::acquire`] a `&Profile` borrowed from a
/// config loaded earlier, and its rotation-lock acquisition WAITS (bounded by
/// [`ROTATION_LOCK_TIMEOUT`], but a wait either way), so an
/// `actions::delete_profile` or `actions::rename_profile` can land in between —
/// leaving the acquire to re-create the profile dir, build a tree, stamp markers
/// and register a live row for an account nothing configures.
///
/// Called as the first act of the acquire's state-flock hold — before the
/// profile directory is re-created, though not before every side effect: the
/// rotation lock file is already open by then, and it is never reaped.
///
/// TWO mechanisms keep the window shut and they are not interchangeable. A
/// SAME-VERSION mutation cannot reach it at all: `acquire` holds its
/// `RotationGuard` through the register-and-stamp window this gate opens, and all
/// three mutation call sites take their own through
/// `actions::rotation_guard_for_mutation`, a
/// `try_acquire` that REFUSES rather than queues. That is the rotation guard's
/// doing, not the flock's — against that actor this gate's placement changes no
/// outcome at all. What the flock placement buys is the mutation holding NO
/// rotation lock: a clauth predating the guard witness on
/// `actions::delete_profile`, where the state flock is the only serialization
/// point the two versions share.
///
/// That actor is posed by `acquire_refuses_a_record_removed_without_a_rotation_lock`
/// through the [`ProfileRuntime::acquire_synced`] seam, which separates a gate
/// moved ABOVE the seam and nothing below it: the seam fires before the flock
/// acquisition, so a gate sitting just outside the hold reads the same
/// post-removal record and refuses identically — measured, it survives a full
/// release run. The `debug_assert!` below is what pins the placement, and it
/// kills both spellings loudly, 27 tests on the DEBUG leg and none on the
/// release one, which is the whole of its reach.
///
/// So do not shorten either scope on the strength of the other.
///
/// It asks the RECORD, not the profile directory. The directory answers a
/// neighbouring question: `unsupported_swap_transport` runs inside this same
/// window on the `--with-fallback` path and `mkdir_700`s the profile root, so a
/// directory-existence gate is satisfied by a start's own leftovers moments
/// after the delete removed them.
///
/// Read through [`crate::profile::is_configured`], which reads the profile list
/// and writes nothing — see there for the ruling that keeps `load_config` out of
/// a flock hold. This reached for `load_config` first, on the argument that both
/// production callers ran it moments earlier so its adopt and rewrite legs were
/// already converged. That argument is refuted one paragraph up: the guard
/// WAITS behind a rotation, and a rotation is precisely what stages a
/// `credentials.json.pending` sidecar for the next load to adopt. The wait's
/// deadline does not soften that: a bounded wait is still a wait.
fn refuse_if_unconfigured(name: &ProfileName) -> Result<()> {
    // Deliberately NOT the `cfg!(test) ||` form its two neighbours in this file
    // carry. Their escape exists because their unit tests drive them with no home
    // sandbox, so demanding the flock would lock the operator's real `~/.clauth`.
    // This has one call site, inside the hold, and no test drives it as a unit —
    // so the flock is already held by construction and the escape would only make
    // the assert dead in the debug test leg, the one place a misplacement gets
    // planted. Debug-only, like every rank check: see `lockorder::holds`.
    debug_assert!(
        crate::lockorder::holds::<crate::lockorder::rank::State>(),
        "the account-record re-read must happen under the state flock, or a \
         mutation holding no rotation lock can land between the read and the hold"
    );
    // The other half of the pair the paragraph above splits, and the FLOOR under
    // the shortened hold: the guard may end with this window but never before it.
    // Same debug-only reach as its neighbour, and the same call site — one, inside
    // both holds — so neither carries the `cfg!(test) ||` escape.
    debug_assert!(
        crate::lockorder::holds::<crate::lockorder::rank::Rotation>(),
        "the account-record re-read must happen under the profile's rotation lock, \
         or a same-version mutation is queued behind nothing"
    );
    if !crate::profile::is_configured(name)
        .with_context(|| format!("failed to re-read the account list while starting '{name}'"))?
    {
        anyhow::bail!(
            "'{name}' was deleted or renamed while this session was starting, \
             run `clauth list` to see the accounts that are left"
        );
    }
    Ok(())
}

impl ProfileRuntime {
    pub(crate) fn acquire(
        profile: &Profile,
        isolation: Isolation,
        stale_env_keys: &[String],
        follows_chain: bool,
    ) -> Result<Self> {
        Self::acquire_synced(
            profile,
            isolation,
            stale_env_keys,
            follows_chain,
            || {},
            |_, _| {},
            || {},
        )
    }

    /// Two injected sync points, both no-ops in production — the same shape
    /// `claude::arm_rolling_from_disk_synced` already uses.
    ///
    /// `pre_lock_done` runs after the rotation guard and immediately before the
    /// state flock, for the regression test that removes the account's record
    /// there, so "the gate reads the record from INSIDE the hold" is pinned by
    /// construction rather than by a thread race nothing can schedule.
    ///
    /// It poses ONE window — a record removal between the rotation guard and the
    /// flock acquisition — and a second thread cannot pose that one, because a
    /// same-version mutation is refused by the rotation guard rather than queued
    /// behind it (see `refuse_if_unconfigured`). It says nothing about the
    /// narrower window on the other side of the flock acquisition; that one wants
    /// a thread holding the flock, and the `debug_assert!` in
    /// `refuse_if_unconfigured` is what covers it today.
    ///
    /// `stamp_window_closing` and `hold_released` are the two halves of the
    /// shortened hold's pin, and they are separate because one question can only be
    /// asked from inside the hold and the other only from outside it.
    ///
    /// `stamp_window_closing` is the LAST statement of the flock closure, handed
    /// this session's own paths and id. It asks whether the two artifacts
    /// `rotation_blocked_for` reads — the marker's flock and the registry row — are
    /// on disk while the rotation lock is still held. Inside the closure is the
    /// only place that question has a stable answer: asked after the drop it cannot
    /// tell "stamped before the lock went" from "stamped just after", so any
    /// hoist out of the closure satisfies it. Measured, both ways.
    ///
    /// The paths and the id rather than the profile name, for the same reason: the
    /// profile-wide `has_live_session` and a `start_profile` scan over the registry
    /// are each satisfied by artifacts this closure stamps for OTHER reasons — the
    /// compat marker, a sibling session's row — so a probe written against them
    /// passes with this session's own marker unclaimed. Measured, it did.
    ///
    /// `hold_released` runs after the drop and asks the one thing the other cannot:
    /// that the lock is free by then. It needs no fixed position now that the
    /// artifacts are pinned inside the closure — restoring the long hold makes it
    /// see the lock HELD wherever it sits.
    fn acquire_synced(
        profile: &Profile,
        isolation: Isolation,
        stale_env_keys: &[String],
        follows_chain: bool,
        pre_lock_done: impl FnOnce(),
        stamp_window_closing: impl FnOnce(&SessionPaths, &SessionId),
        hold_released: impl FnOnce(),
    ) -> Result<Self> {
        let name = &profile.name;
        let claude_home = claude_dir()?;
        if !claude_home.exists() {
            anyhow::bail!("~/.claude not found; install Claude Code first");
        }
        let canonical = canonical_credentials(name)?;
        let profile_root = profile_dir(name)?;

        // Hold the per-profile rotation lock across the session-stamp window so
        // a concurrent `oauth::rotate_one_inner` for this profile cannot spend the
        // single-use refresh token while we are starting up. Ordering rule
        // (matches `oauth::rotate_one_inner`): RotationGuard OUTERMOST, then the
        // state flock inside.
        //
        // Bounded, unlike every other blocking acquisition of this lock: this is
        // the one with a caller waiting on it and no channel to say "still
        // queued" — an operator at a spinner, or an MCP `delegate` whose pre-spawn
        // window emits no progress notification — so an unbounded park is
        // indistinguishable from a hang. `ROTATION_LOCK_TIMEOUT` derives the
        // deadline from what a healthy holder can spend.
        let rotation_guard = RotationGuard::acquire_with_timeout(name, rotation_lock_timeout())?;
        pre_lock_done();

        let (session, paths, pid_lock, legacy_lock, mode) = with_state_lock(|_held| {
            // Inside the hold, and ahead of every write this closure does — see
            // `refuse_if_unconfigured` for which mechanism each half buys.
            refuse_if_unconfigured(name)?;
            // The transport is probed FIRST: under `LinkMode::Fake` the tree is
            // shared under the bare stem, so the mode decides every path below.
            // The profile dir is the probe site because it exists independently
            // of the tree — created here rather than assumed, so nothing rests on
            // `RotationGuard::acquire` having made it.
            crate::profile::mkdir_700(&profile_root)
                .with_context(|| format!("failed to create {}", profile_root.display()))?;
            let mode = detect_link_mode(&profile_root)?;
            // A sid is a NAME, not a claim. `<pid>-<seq>` collides only when a
            // second LIVE process minted the same pair, which needs the shapes
            // `stamp_legacy_marker` names: a `~/.clauth` shared across pid
            // namespaces, or an NFS home. Under `LinkMode::Fake` that collision
            // lands on this session's OWN marker, because the bare-stem tree puts
            // it at the compat path, so there is no separate marker to fall back
            // to and no `try_lock` concede on the way in. Re-mint rather than
            // wait: the claim below runs inside the state flock, so a blocking
            // wait there wedges every other clauth process on this home, and
            // `is_session_alive` reads every unknown as live, so an unreadable
            // marker moves this session aside instead of parking it.
            let mut session = SessionId::mint();
            let mut paths = SessionPaths::resolve(name, isolation, &session, mode)?;
            for _ in 0..SID_COLLISION_REMINTS {
                if !is_session_alive(&paths.pid_file) {
                    break;
                }
                session = SessionId::mint();
                paths = SessionPaths::resolve(name, isolation, &session, mode)?;
            }
            let SessionPaths {
                runtime,
                sessions,
                pid_file,
                legacy_marker,
            } = &paths;

            crate::profile::mkdir_700(sessions)
                .with_context(|| format!("failed to create {}", sessions.display()))?;
            // An unknown reads as live, so the wipe below is skipped rather than
            // aimed at a tree this probe could not clear. The build is additive,
            // so declining to wipe is always the recoverable direction, and an
            // `sessions` dir this could not read still fails loudly at the
            // `open_pid_file` + `lock` below.
            let active = prune_stale_sessions(sessions).unwrap_or(1);
            // Nothing live in this session's marker dir, yet a tree already sits
            // at its path: a dead session's leftovers under a recycled pid, or —
            // under the shared tree — a whole profile's worth nobody is using.
            // Rebuild from scratch so stale symlinks/copies to entries that have
            // since vanished from ~/.claude/ don't carry over. A live sibling
            // holds a marker here, so its tree is never the one wiped.
            //
            // The converse does NOT hold, and two concurrent starts on one
            // Windows host can land on different modes. A live REAL
            // session's compat marker sits in this same shared dir, so it makes
            // `active` nonzero for a Fake acquire and suppresses the wipe of a
            // bare `runtime/` that session does not use. A stale pre-upgrade tree
            // is then adopted rather than rebuilt, and since the build is
            // additive, a symlink forest stays symlinks under `mode == Fake`.
            // Benign — reading a symlink needs no privilege — but it is why this
            // wipe cannot be relied on as the only staleness cure.
            if active == 0 && runtime.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(runtime)
                    .with_context(|| format!("failed to clear {}", runtime.display()))?;
            }
            crate::profile::mkdir_700(runtime)
                .with_context(|| format!("failed to create {}", runtime.display()))?;
            build_runtime_dir_with_active_env(
                runtime,
                &claude_home,
                profile,
                &canonical,
                mode,
                isolation,
                stale_env_keys,
            )?;
            let file = open_pid_file(pid_file)
                .with_context(|| format!("failed to open {}", pid_file.display()))?;
            // `try_lock`, not `lock`, for the reason the re-mint loop above
            // states. Reaching here means the loop spent its re-mints against a
            // holder that outlived every one of them, so failing loudly is the
            // only honest end: waiting would park the state flock.
            if let Err(e) = file.try_lock() {
                anyhow::bail!(
                    "failed to claim session marker {}: {e}. Another live process \
                     holds this session id",
                    pid_file.display()
                );
            }
            let legacy_lock = legacy_marker.as_deref().and_then(stamp_legacy_marker);

            // Register inside this same hold, once the marker is flock-held: the
            // row can then never exist without a liveness signal for GC to test
            // it by, and `register`'s own `with_state_lock` takes the reentrant
            // path instead of a second 25s-bounded flock acquisition. A registry
            // failure is reported and stepped over — the session itself is
            // already sound, and failing here would trade a missing row for a
            // dead session.
            let opt_in =
                chain_opt_in_survives(follows_chain, isolation, mode, cfg!(target_os = "macos"));
            // A clamp here means the opt-in asked for something this host's probed
            // mode cannot support, so the session runs without the chain its caller
            // asked for — the silent non-switch the flag exists to prevent. Say so
            // rather than dropping it quietly.
            if follows_chain && !opt_in {
                logline!(
                    "clauth: '{name}' cannot follow the fallback chain on this host; \
                     the session stays on its launch account"
                );
            }
            let row = crate::live_sessions::LiveSession::starting(
                &session,
                name,
                isolation == Isolation::Isolated,
                opt_in,
                // The SAME value the runtime tree is built from below, so the
                // row cannot disagree with what this session actually reads —
                // on macOS, which is the only place it is consulted. The
                // stronger "never" would need `swap_to` to update it when it
                // moves `cell.canonical`, and it deliberately does not: swaps
                // refuse macOS (`swap_support`), and macOS is where
                // `live_session_holds_rotatable` reads this. A platform that
                // gains both swaps and the rotation refusal inherits that
                // update as a prerequisite.
                Some(canonical.clone()),
            );
            if let Err(e) = crate::live_sessions::register(&row) {
                logline!("clauth: registering the live session failed: {e}");
            }
            stamp_window_closing(&paths, &session);
            Ok::<_, anyhow::Error>((session, paths, file, legacy_lock, mode))
        })?;
        // Released at the end of the register-and-stamp window rather than at the
        // end of this function, so a queued peer waits out that window and not the
        // watchdog arming behind it.
        //
        // What the hold must still cover, and does: the credential
        // materialization inside `build_runtime_dir_with_active_env` (which
        // samples the chain — a byte copy under `LinkMode::Fake`, a relink plus a
        // possible adopt under `Real`), the marker `try_lock`, and the registry
        // row. Those last two are the whole of what `rotation_blocked_for` reads
        // — `has_live_session` walks the marker dirs, `live_session_holds_rotatable`
        // reads the row's `launch_store` — so a rotation taking this lock one
        // instruction after the drop already sees this session and refuses on
        // macOS exactly as before. They are also what the `has_live_session` gate
        // on rename and disable reads, so those two are refused in the gap by
        // that gate rather than by this lock. Delete is the exception and stays
        // one: `actions::delete_profile` reads that gate only when `!force`, so a
        // `--force` delete lands in the gap where the base's `try_acquire` would
        // have refused it. `--force` against a live session is an outcome its
        // operator already owns; the gap only moves it earlier.
        //
        // What sits past it needs none of it, which is why the shorter scope
        // holds: `SessionSwap::new` is construction over values already computed;
        // `watch_specs` + `try_start` arm an FS watcher over paths and touch no
        // credential; and the watchdog thread starts with an empty rank stack and
        // takes its OWN `with_state_lock` per tick, so it never ran under this
        // guard even when the hold reached this far. No token can be double-spent
        // across the gap: every leg that spends takes this same lock first, and
        // the acquire itself spends nothing — it reads and links, never refreshes.
        //
        // The gap is not free. `watchdog::run_with_watcher`
        // records that a write landing while the watcher is still arming (18-34 ms
        // on macOS, per its own measurement) produces no event, so an armed
        // watcher then waits out its whole 30 s fallback; the hold reaching past
        // `try_start` was one of the two things making that race unwritable, and
        // this drop gives it up. Reachable by a same-profile holder that writes on
        // pure disk right after taking the lock: `claude::arm_rolling_from_disk`'s
        // sidecar stamp, `SessionSwap::swap_to`'s relink, and
        // `oauth::gate_under_guard`'s `roll_from_stored_chain` leg, which stamps
        // from the chain already on disk with no HTTP in front of it. A rotation
        // that refreshes is the one shape that cannot: its write comes after a
        // network round trip. It costs nothing under
        // `LinkMode::Real`, where the runtime credential file is a symlink onto
        // the store and a write needs no reconcile at all; under `Fake` it is up
        // to 30 s of stale mirrored bytes. `Real` is the norm on unix and `Fake`
        // the norm on Windows, so the exposure is mostly Windows'; macOS reaches
        // it only through `detect_link_mode`'s failure arm — `try_real_symlink`
        // is a real `symlink(2)`, not an infallible call — which is a `$HOME` on a
        // volume without symlink support or a denial on the probe write.
        drop(rotation_guard);
        hold_released();
        // Built from `paths` rather than from locals moved out of it, so the
        // runtime dir the swap repoints and the marker it must recognize as its
        // own come from one source.
        let swap = std::sync::Arc::new(SessionSwap::new(
            session, isolation, mode, profile, canonical, &paths,
        ));
        let SessionPaths {
            sessions,
            pid_file,
            legacy_marker,
            ..
        } = paths;

        // This session's three reconcile legs, as one value the watchdog loop
        // calls back into.
        struct WatchdogLegs {
            claude_home: PathBuf,
            swap: std::sync::Arc<SessionSwap>,
        }
        impl crate::watchdog::Reconcile for WatchdogLegs {
            fn config(&self) {
                if let Err(e) = crate::claude_json::sync_once() {
                    logline!("clauth: .claude.json sync failed: {e}");
                }
                if let Err(e) = crate::settings_sync::sync_once() {
                    logline!("clauth: settings.json sync failed: {e}");
                }
            }
            fn credentials(&self) {
                if let Err(e) = tick(&self.claude_home, &self.swap) {
                    logline!("clauth: watchdog tick failed: {e}");
                }
            }
            fn swap_poll(&self) {
                self.swap.poll();
            }
        }

        let (watchdog_tx, watchdog_rx) = crossbeam_channel::bounded::<()>(1);
        let legs = WatchdogLegs {
            claude_home: claude_home.clone(),
            swap: std::sync::Arc::clone(&swap),
        };
        // Armed HERE rather than on the spawned thread, so that `acquire`
        // returning IS the barrier proving the watch is live. Arming costs
        // 18-34 ms on macOS (FSEvents resolves and registers each directory),
        // and a caller that spawned first had no way to learn when its watch
        // went up: a credential write landing in that window produced no event
        // and waited out the whole 30 s fallback. It is outside the rotation-lock
        // hold, which the drop above ends at the register-and-stamp window, so a
        // same-profile peer queued behind this start no longer waits it out.
        let specs = crate::watchdog::watch_specs(
            swap.runtime.as_path(),
            swap.canonical().as_path(),
            &claude_home,
        );
        let requested = specs.len();
        let watcher = crate::watchdog::try_start(&specs, crate::watchdog::PRODUCTION.debounce);
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let watchdog_handle = thread::Builder::new()
            .name(format!("clauth-wdog-{name}"))
            .spawn(move || {
                // Event-driven reconcile, polling only where events are
                // unavailable. Exits when the shutdown sender is dropped (see
                // ProfileRuntime::Drop).
                crate::watchdog::run_with_watcher(
                    watcher,
                    requested,
                    &watchdog_rx,
                    &crate::watchdog::PRODUCTION,
                    &legs,
                );
            })
            .expect("failed to spawn watchdog thread");

        Ok(Self {
            swap,
            pid_file,
            legacy_marker,
            claude_home,
            sessions,
            _pid_lock: pid_lock,
            legacy_lock,
            watchdog_signal: Some(watchdog_tx),
            watchdog_handle: Some(watchdog_handle),
        })
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.swap.runtime
    }

    /// This session's swap executor. Production reaches it through the watchdog
    /// thread's own clone; this accessor exists so a test can drive one leg at a
    /// time instead of racing a 1 Hz tick.
    #[cfg(all(test, not(target_os = "macos")))]
    fn swap(&self) -> &SessionSwap {
        &self.swap
    }

    /// This session's liveness-marker dir. Holds only its own marker under real
    /// symlinks; under [`LinkMode::Fake`] it is shared with every other session
    /// of this profile+flavor, so the dir can hold several.
    ///
    /// Its live count gates anything that MOVES state out of `config_dir` — the
    /// count, not the keying, is what proves no Claude Code is reading the tree
    /// being emptied. Under the shared tree that gate genuinely fires: a caller
    /// that moves state out (`start::rescue_teardown`) does nothing until the
    /// LAST session of the profile leaves.
    pub(crate) fn sessions_dir(&self) -> &Path {
        &self.sessions
    }
}

/// How many additional state-flock acquisitions teardown makes after its first
/// timed out. A timed-out acquire here is a wedged peer holding the flock past
/// the deadline, not a permissions fault, so retrying the SAME hold is the
/// recovery and splitting it is not. The daemon's watchdog aborts a wedged main
/// loop within 30 s of the wedge — inside the second 25 s acquire — so the FIRST
/// retry is the common recovery. Two retries covers a slow abort without holding
/// the exiting session open past ~75 s, after which the next run's GC is the
/// fallback.
const TEARDOWN_ACQUIRE_RETRIES: u32 = 2;

/// Take the state flock for teardown, retrying a
/// [`crate::lock::StateLockTimeout`] up to [`TEARDOWN_ACQUIRE_RETRIES`]
/// additional times. Only the timeout is retried: it names a wedged peer, which
/// heals when that peer is killed or its hold ends; an IO error (permissions, a
/// broken tree) does not heal and propagates on the first failure. The caller
/// runs the whole teardown body once inside the returned guard, so the
/// single-hold invariant in `Drop` holds across the retry — the body is never
/// split across two acquisitions.
fn acquire_state_lock_for_teardown() -> Result<crate::lock::StateLock> {
    let mut retries = 0u32;
    loop {
        match crate::lock::StateLock::acquire_with_timeout(crate::lock::state_lock_timeout()) {
            Ok(guard) => return Ok(guard),
            Err(e)
                if retries < TEARDOWN_ACQUIRE_RETRIES
                    && e.downcast_ref::<crate::lock::StateLockTimeout>().is_some() =>
            {
                #[cfg(test)]
                on_teardown_acquire_timeout();
                retries += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

// Test seam: fires once per timed-out teardown acquire, BEFORE the retry, so a
// test can release a held flock between the first timeout and the retry (posing
// "a wedged peer released") or count the retries (pinning the bound) without
// sleeping the real 25 s. `cfg(test)`-only; no production path sets it.
#[cfg(test)]
thread_local! {
    static TEARDOWN_TIMEOUT_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn on_teardown_acquire_timeout() {
    TEARDOWN_TIMEOUT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
}

#[cfg(test)]
fn set_teardown_timeout_hook(hook: Option<Box<dyn FnMut()>>) {
    TEARDOWN_TIMEOUT_HOOK.with(|h| *h.borrow_mut() = hook);
}

impl Drop for ProfileRuntime {
    fn drop(&mut self) {
        // Before the signal, not after: the watchdog may be mid-tick, and a swap
        // STARTED from here would hold this join for the state-lock timeout plus
        // an unbounded rotation-flock wait.
        self.swap.shutdown.begin();
        // Drop the sender to signal the watchdog, then join.
        drop(self.watchdog_signal.take());
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }

        if let Err(e) = tick(&self.claude_home, &self.swap) {
            logline!("clauth: final sync failed: {e}");
        }

        // Flush this session's last `.claude.json` / `settings.json` changes to
        // the global files and siblings before a possible teardown removes this
        // runtime's copies.
        if let Err(e) = crate::claude_json::sync_once() {
            logline!("clauth: final .claude.json sync failed: {e}");
        }
        if let Err(e) = crate::settings_sync::sync_once() {
            logline!("clauth: final settings.json sync failed: {e}");
        }

        // One hold for the whole teardown. `unregister` takes the state lock
        // itself, so calling it out here would be a second top-level acquisition
        // — two 25s-bounded flock waits back to back, with a window between them
        // where the row is gone but the marker is not. A timed-out acquire is a
        // wedged peer, not a permissions fault, so the SAME hold is retried
        // (bounded, via `acquire_state_lock_for_teardown`) rather than split:
        // the retry re-enters the same flock for the same body.
        let legacy_lock = self.legacy_lock.take();
        match acquire_state_lock_for_teardown() {
            Ok(_guard) => {
                if let Err(e) = crate::live_sessions::unregister(self.swap.session.as_str()) {
                    logline!("clauth: unregistering the live session failed: {e}");
                }
                if let Err(e) = std::fs::remove_file(&self.pid_file)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    logline!("clauth: remove pid file failed: {e}");
                }
                // A `None` marker is a session whose own `pid_file` IS the compat
                // path, already unlinked above — there is no second file, so this
                // whole leg is skipped rather than special-cased inside it.
                if let Some(legacy_marker) = self.legacy_marker.as_deref() {
                    // Only unlink a marker this session actually owns. `legacy_lock`
                    // is `None` when `try_lock` lost to a live process that minted
                    // the same sid — unlinking there would delete a FOREIGN session's
                    // liveness signal, which is the same rotation-burn this marker
                    // exists to prevent. Release before unlinking, so a sibling's
                    // `prune_stale_sessions` never reads a removed path.
                    if legacy_lock.is_some() {
                        drop(legacy_lock);
                        let _ = std::fs::remove_file(legacy_marker);
                    }
                    // The compat dir is shared by every session of this
                    // profile+flavor, so it goes only once the last has released.
                    if let Some(legacy_dir) = legacy_marker.parent()
                        && prune_stale_sessions(legacy_dir).unwrap_or(1) == 0
                    {
                        let _ = std::fs::remove_dir(legacy_dir);
                    }
                }
                // Every member a swap moved this session onto holds markers of its
                // own, in both layouts. A dead session that keeps one blocks rotation
                // on an account nothing is using.
                self.swap.release_swapped_markers();
                let still_active = prune_stale_sessions(&self.sessions).unwrap_or(1);
                if still_active == 0 {
                    let _ = std::fs::remove_dir_all(&self.swap.runtime);
                    let _ = std::fs::remove_dir(&self.sessions);
                }
            }
            Err(e) => logline!("clauth: drop cleanup failed: {e}"),
        }
    }
}

/// A [`Command`](std::process::Command) for the `claude` CLI, resolved so an
/// npm-installed shim launches on Windows too. Rust's bare `Command::new`
/// appends only `.exe` and skips `PATHEXT`, so a `claude.cmd`/`claude.bat` (npm
/// global) is invisible and `start`/`delegate` fail with "program not found"
/// even though the user runs `claude` fine by hand. `which_all` enumerates every
/// `PATHEXT` match in `PATH` order; we prefer a native `.exe` over a `.cmd`/
/// `.bat` shim whenever both resolve (the shim adds a cmd.exe hop, and PATH dir
/// order could otherwise surface it first), else take the first match and let
/// std route it through cmd.exe with hardened escaping (post-CVE-2024-24576).
/// Unix keeps the bare lookup.
/// clauth-owned env keys that must reach the spawned `claude` only via the
/// target profile's runtime `settings.json`, never inherited from the parent
/// process. A parent `claude` running profile A had these written into its own
/// `settings.json.env`, which Claude Code applies to `process.env` at startup;
/// without scrubbing they leak across profiles and re-route the spawned session
/// to A's endpoint or account.
pub(crate) const MANAGED_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

/// Drop [`MANAGED_ENV_KEYS`] plus the outgoing activation's custom env keys
/// ([`crate::actions::outgoing_env_keys`]: the active profile's, or every
/// configured profile's with no marker to read) from `command`'s inherited
/// env, so the target's runtime `settings.json` is the sole source for them.
/// Shared by `clauth start` and the MCP delegate. Call before layering any
/// caller-supplied env, so a caller can still set a key back deliberately.
pub(crate) fn scrub_profile_env(command: &mut std::process::Command, stale_env_keys: &[String]) {
    for key in MANAGED_ENV_KEYS {
        command.env_remove(key);
    }
    for key in stale_env_keys {
        command.env_remove(key);
    }
}

/// True when `dir` resolves to the real `$HOME`. `CLAUDE_CONFIG_DIR` only
/// relocates Claude Code's USER-tier settings source; the PROJECT tier is a
/// wholly separate `<cwd>/.claude/settings.json` lookup with no ancestor walk,
/// and it outranks the user tier on any key it defines. When the spawned
/// `claude`'s cwd is exactly `$HOME`, `<cwd>/.claude/` IS the real
/// `~/.claude/` — the file clauth itself writes for whichever profile is
/// globally active — so that profile's `env` silently overrides the target's.
/// Canonicalizes both sides so a symlinked `$HOME` still matches.
fn cwd_is_real_home(dir: &Path) -> bool {
    let Ok(home) = home_dir() else {
        return false;
    };
    match (dir.canonicalize(), home.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => dir == home,
    }
}

/// When `cwd` resolves to the real `$HOME`, append `--setting-sources user` so
/// Claude Code skips the project/local settings tiers entirely (their lookup
/// is cwd-based, and `<$HOME>/.claude/` is the same directory as the real
/// user-tier settings). Elsewhere a project's own committed
/// `.claude/settings.json` (permissions, hooks, statusline) still applies, as
/// today. `cwd` is the resolved directory the spawned `claude` will actually
/// run in — the caller's explicit cwd override if any, else the process's own
/// current directory.
pub(crate) fn guard_home_project_settings(command: &mut std::process::Command, cwd: &Path) {
    if cwd_is_real_home(cwd) {
        command.arg("--setting-sources").arg("user");
    }
}

pub(crate) fn claude_command() -> std::process::Command {
    #[cfg(windows)]
    if let Ok(matches) = which::which_all("claude") {
        let all: Vec<std::path::PathBuf> = matches.collect();
        let chosen = all
            .iter()
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")))
            .or_else(|| all.first());
        if let Some(path) = chosen {
            return std::process::Command::new(path);
        }
    }
    std::process::Command::new("claude")
}

/// Test-only [`detect_link_mode`] override. `try_real_symlink` always succeeds
/// on unix, so the fake-symlink transport — and the shared bare-stem tree it
/// selects — is otherwise unreachable from a Linux/macOS test run. Serialized by
/// `profile::HOME_TEST_LOCK`, which every test that sets it already holds via
/// `with_fake_home`. Never compiled into the binary.
#[cfg(test)]
static LINK_MODE_OVERRIDE: std::sync::Mutex<Option<LinkMode>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn set_link_mode_override(mode: LinkMode) {
    if let Ok(mut guard) = LINK_MODE_OVERRIDE.lock() {
        *guard = Some(mode);
    }
}

#[cfg(test)]
fn clear_link_mode_override() {
    if let Ok(mut guard) = LINK_MODE_OVERRIDE.lock() {
        *guard = None;
    }
}

/// Probe the OS by attempting a real symlink in `probe_dir`. Anything other than
/// success — privilege denial, unsupported filesystem, the
/// `cfg(not(any(unix, windows)))` fallback — drops to fake-symlink mode.
///
/// Pointed at the PROFILE dir, not the runtime tree: the mode decides the tree's
/// name ([`paired_dir_names`]), so it has to be known before that dir exists. The
/// two dotfiles below match no `runtime*`/`sessions*` predicate, so GC and every
/// enumeration step over them.
fn detect_link_mode(probe_dir: &Path) -> Result<LinkMode> {
    #[cfg(test)]
    if let Some(mode) = LINK_MODE_OVERRIDE.lock().ok().and_then(|guard| *guard) {
        return Ok(mode);
    }
    let probe_target = probe_dir.join(".clauth-probe-target");
    let probe_link = probe_dir.join(".clauth-probe-link");
    let _ = std::fs::remove_file(&probe_target);
    let _ = std::fs::remove_file(&probe_link);
    std::fs::write(&probe_target, b"")
        .with_context(|| format!("failed to write {}", probe_target.display()))?;
    let mode = match try_real_symlink(&probe_target, &probe_link) {
        Ok(()) => LinkMode::Real,
        Err(_) => LinkMode::Fake,
    };
    let _ = std::fs::remove_file(&probe_link);
    let _ = std::fs::remove_file(&probe_target);
    Ok(mode)
}

#[cfg(unix)]
fn try_real_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn try_real_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn try_real_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no symlink support",
    ))
}

/// Walk `sessions/`, drop entries whose owner has died, return the live count.
/// Caller holds the cross-process state lock so two simultaneous starts can't
/// both conclude "no other sessions" and tear down the runtime under each other.
///
/// Same `Some(0)`-is-absent / `None`-is-unknown shape as [`live_marker_names`],
/// and the reason is sharper here because this is the DESTRUCTIVE level: all
/// three callers turn a zero into `remove_dir_all` of a runtime tree, which under
/// [`LinkMode::Fake`] is shared by every session of the profile+flavor. An
/// unreadable dir, or an entry that cannot be read, is therefore an unknown —
/// folding either into a zero would hand a live session's tree to the sweep.
fn prune_stale_sessions(sessions: &Path) -> Option<usize> {
    let entries = match std::fs::read_dir(sessions) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    let mut alive = 0;
    for entry in entries {
        let path = entry.ok()?.path();
        if is_session_alive(&path) {
            alive += 1;
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    Some(alive)
}

fn is_session_alive(pid_file: &Path) -> bool {
    // Open without O_CREAT: creating the file would race with another session
    // that just created it but hasn't locked it yet, producing a false
    // "unlocked = dead" reading. try_lock succeeds iff no other open fd holds
    // an exclusive flock, i.e. the previous owner has exited.
    //
    // Only a genuinely absent marker is dead. Every other `open` failure —
    // EMFILE under fd pressure, ESTALE on an NFS home, an EACCES from a mode
    // change — is an unknown, and `prune_stale_sessions` UNLINKS whatever this
    // reads as dead. Folding one into a false would delete a live session's
    // marker, then let the rotation leg spend the single-use refresh token that
    // session still holds.
    let file = match OpenOptions::new().read(true).write(true).open(pid_file) {
        Ok(file) => file,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    // Any I/O error: treat as alive so we don't race a live session.
    file.try_lock().is_err()
}

/// Build or incrementally update the runtime tree.
///
/// The walk is additive rather than a clean build: entries whose runtime
/// counterpart already exists are skipped. That keeps it correct over a tree the
/// acquire above declined to wipe, and keeps a rebuild after a `~/.claude/`
/// addition from disturbing the rest.
///
/// Shared vs. per-profile layout:
/// - **Shared via symlink/copy across all profiles:** every top-level entry
///   in `~/.claude/` except `settings.json` and `.credentials.json` —
///   this includes `projects/`, `todos/`, `statsig/`, `sessions/`, `cache/`,
///   `commands/`, `plugins/`, `tasks/`, `teams/`, `hooks/`, `history.jsonl`,
///   and similar. Claude Code treats these as user-global state so sharing is
///   intentional; per-profile isolation would hide project history and
///   installed commands.
/// - **Per-profile:** `settings.json` (merged with profile overrides),
///   `.credentials.json` (the profile's own OAuth token chain), and
///   `.claude.json` (a copy seeded from `~/.claude.json`). Settings are
///   rewritten when changed; credentials are reconciled without using the
///   shared `~/.claude/.credentials.json` copy; `.claude.json` is reconciled
///   across all profiles by `crate::claude_json`, which propagates every field
///   except the account-specific ones (`oauthAccount` + billing caches).
///
/// In [`Isolation::Isolated`] mode NOTHING under `~/.claude/` is linked — the
/// tree holds only the reconciled credentials, the empty-base `settings.json`,
/// and the seeded `.claude.json`. A clean session thus shares no operator state
/// and, critically, no writable store: its CC (empty settings → default
/// `cleanupPeriodDays`) can never write or clean the operator's `projects/`.
///
/// `stale_env_keys` (the outgoing activation's custom env: the active
/// profile's, or every configured profile's with no marker to read) are
/// stripped from the shared `settings.json` base before this profile's
/// overrides are merged, so a `clauth start <other>` session does not inherit
/// a departed account's custom `[env]`. Model + endpoint keys are re-derived
/// per profile in `build_claude_settings_json`, so only custom `[env]` needs
/// this strip.
fn build_runtime_dir_with_active_env(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    canonical: &Path,
    mode: LinkMode,
    isolation: Isolation,
    stale_env_keys: &[String],
) -> Result<()> {
    // Drop any top-level symlink whose `~/.claude/` target has vanished before
    // the re-walk. A prior session's link can dangle once the operator moves the
    // source aside (the reported `runtime/CLAUDE.md` → moved memory case); the
    // walk below only visits entries still in `~/.claude/`, so it would never
    // revisit — and skip — that stale link. Live entries stay; a still-present
    // source gets re-linked by the walk.
    prune_dangling_links(runtime)?;

    let mut pending: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(claude_home)
        .with_context(|| format!("failed to read {}", claude_home.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        if file_name == "settings.json" || file_name == ".credentials.json" {
            continue;
        }
        // A `copy_file` publish in flight, not content. `union_children` skips
        // these for the watchdog mirror; this walk has the same exposure and a
        // sharper consequence, because it PROPAGATES its error — the whole
        // `acquire` fails instead of a tick that would have re-converged.
        //
        // The shared fake-mode tree is where it bites: the watchdog's lockless
        // `mirror_tree` publishes a runtime-side file back into `~/.claude`
        // while a sibling session is acquiring, and on Windows the publishing
        // thread still has the staging file OPEN, so the copy fails with
        // "used by another process". Linking one in real mode is no better —
        // it lands a link to a path that is about to be renamed away.
        if crate::watchdog::is_staging(&file_name) {
            continue;
        }
        // Isolated owns its writable state — link NOTHING from ~/.claude. A clean
        // session's CC runs with an empty settings.json (default
        // `cleanupPeriodDays`), so a shared `projects/` symlink would let it delete
        // the operator's transcripts down to 30 days. CC recreates what it needs in
        // the throwaway tree; creds/settings/.claude.json are seeded below.
        if isolation == Isolation::Isolated {
            continue;
        }
        let dst = runtime.join(&file_name);
        if dst.symlink_metadata().is_ok() {
            continue;
        }
        pending.push((entry.path(), dst));
    }
    materialize_entries(pending, mode)?;
    write_merged_settings(runtime, claude_home, profile, isolation, stale_env_keys)?;

    let creds_link = runtime.join(".credentials.json");
    reconcile_credentials(&creds_link, canonical, mode)?;

    seed_claude_json(runtime, claude_home)?;

    Ok(())
}

/// Test-only convenience over [`build_runtime_dir_with_active_env`]: passes
/// an empty strip list, because inline runtime tests build a tree for the one
/// profile under test and have no other profile's `[env]` to keep out. The
/// empty list is NOT the production no-marker shape — with no marker to read,
/// production strips every configured profile's keys.
#[cfg(test)]
fn build_runtime_dir(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    canonical: &Path,
    mode: LinkMode,
    isolation: Isolation,
) -> Result<()> {
    build_runtime_dir_with_active_env(
        runtime,
        claude_home,
        profile,
        canonical,
        mode,
        isolation,
        &[],
    )
}

/// Remove top-level symlinks in the runtime whose target no longer resolves
/// (the `~/.claude/` source was moved or deleted). Self-heals the dangling-link
/// artifact a prior build can leave; only symlinks are touched — regular files
/// and directories are never removed, and a link is removed only once its
/// target is already gone. `.credentials.json` is reconciled separately
/// afterwards, so pruning a stale one here is safe.
fn prune_dangling_links(runtime: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(runtime) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = path.symlink_metadata()
            && meta.file_type().is_symlink()
            && !path.exists()
        {
            // Windows splits link removal by what the link POINTS AT, not by
            // what the link is: `remove_file` clears a dangling FILE symlink but
            // answers os error 5 on a dangling junction or directory symlink and
            // leaves it standing (measured on Windows 11, elevated and with
            // `SeCreateSymbolicLinkPrivilege` stripped alike). A survivor is
            // permanent, not cosmetic — the re-walk below skips any entry whose
            // `symlink_metadata` succeeds, so that name never re-materializes.
            //
            // `remove_dir`, never `remove_dir_all`: it unlinks the link itself
            // and refuses a non-empty directory, and the guard above already
            // proved the target is gone, so there is nothing behind this link
            // for either call to reach. `rmdir` is additionally believed unable
            // to traverse a LIVE link on either platform, which would make the
            // call safe without the guard — unverified, so the guard is what
            // this rests on. Its one soft edge: `Path::exists` swallows every
            // stat error, so a live link over a dropped mount reads as dangling
            // and gets unlinked. The re-walk re-links it on the same pass.
            if let Err(file_err) = std::fs::remove_file(&path)
                && let Err(dir_err) = std::fs::remove_dir(&path)
            {
                logline!(
                    "clauth: stale link {} could not be removed ({file_err}; as a dir: {dir_err})",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

/// Compute this profile's merged `settings.json` and write it into the runtime
/// tree only when absent or byte-different, so a rebuild over an existing tree
/// leaves an already-correct file's mtime alone (the reconcilers below key on
/// it). Isolated mode builds from an empty base (no operator
/// hooks/permissions/statusline/plugin config), keeping only the profile's own
/// env + model routing.
///
/// `stale_env_keys` (the outgoing activation's custom env: the active
/// profile's, or every configured profile's with no marker to read) are
/// stripped from the shared base first, so a `clauth start <other>` session
/// does not inherit a departed account's custom `[env]`. Model + endpoint keys
/// are re-derived per profile in `build_claude_settings_json`, so only custom
/// `[env]` needs this. Starting the active profile itself passes its own keys,
/// which the merge re-inserts (a no-op strip).
///
/// This computes the copy; `crate::settings_sync` then keeps it converged with
/// the base and every sibling runtime for the session's lifetime. The two agree
/// by construction: the syncer writes shared fields back into the base, so this
/// recompute reproduces the same bytes on the next start instead of undoing it.
fn write_merged_settings(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    isolation: Isolation,
    stale_env_keys: &[String],
) -> Result<()> {
    let settings_src = claude_home.join("settings.json");
    let base = match isolation {
        Isolation::Shared => Some(settings_src.as_path()),
        Isolation::Isolated => None,
    };
    let merged = build_claude_settings_json(base, profile, stale_env_keys)?;
    let settings_dst = runtime.join("settings.json");
    // This file carries the api-key profile's top-level `apiKeyHelper` command
    // string (plus the base_url/model env keys), so it must land 0o600 like
    // every other clauth-owned write. The raw key itself lives in `config.toml`
    // (minted per request by the helper); the runtime settings.json is still
    // operator-sensitive. The write gate also fires when only the mode is wrong
    // (a byte-identical file an older build left at the umask never self-heals
    // otherwise).
    let needs_write = match std::fs::read(&settings_dst) {
        Ok(existing) => existing != merged.as_bytes() || !is_owner_only(&settings_dst),
        Err(_) => true,
    };
    if needs_write {
        atomic_write_600(&settings_dst, merged).context("failed to write runtime settings.json")?;
    }
    Ok(())
}

/// True when `path`'s mode is exactly 0o600 on Unix. Always true on non-Unix
/// (no POSIX modes), so the settings write-gate keys on bytes there.
fn is_owner_only(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777 == 0o600)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// Seed this profile's private copy of `~/.claude.json`. Claude Code's big
/// config file embeds an account-specific `oauthAccount` block (plus billing
/// caches) that must NOT be shared across profiles — CC trusts the cached
/// identity and won't re-derive it from the token on a normal startup, so a
/// shared symlink leaks one account's identity into another. The background
/// syncer (`crate::claude_json`) keeps the non-per-profile fields converged
/// across all copies (latest write wins). A freshly seeded copy strips the
/// global file's `oauthAccount` (issue #17: a raw copy is born carrying
/// whichever account was active at seed time, wrong for every profile but the
/// active one) so this profile starts identity-less and Claude Code re-derives
/// it from THIS profile's own credentials on first boot; that boot (or the
/// next OAuth login) writes the correct identity, which the syncer then
/// preserves as this copy's own per-profile field.
///
/// Seeds from the global file when this profile has no real copy yet, or
/// migrates the old shared symlink (pre-per-profile behavior) to a copy.
/// `atomic_write_600` renames over the path, replacing a symlink in one step —
/// no window where a sibling session sees the file missing — at owner-only mode
/// (the seed carries the account's `oauthAccount` billing/identity caches).
/// Existing real copies keep their own identity and synced shared fields.
fn seed_claude_json(runtime: &Path, claude_home: &Path) -> Result<()> {
    let Some(home) = claude_home.parent() else {
        return Ok(());
    };
    let global = home.join(".claude.json");
    let dst = runtime.join(".claude.json");
    let is_symlink = dst
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink());
    if (is_symlink || !dst.exists())
        && let Ok(bytes) = std::fs::read(&global)
    {
        let bytes = strip_oauth_account_on_seed(bytes);
        atomic_write_600(&dst, &bytes)
            .with_context(|| format!("failed to seed {}", dst.display()))?;
    }
    Ok(())
}

/// Remove `oauthAccount` from freshly seeded `.claude.json` bytes. A no-op
/// (returns the bytes unchanged) when the key is already absent or the source
/// doesn't parse as a JSON object, so the common case stays a plain byte copy.
fn strip_oauth_account_on_seed(bytes: Vec<u8>) -> Vec<u8> {
    let Ok(serde_json::Value::Object(mut obj)) =
        serde_json::from_slice::<serde_json::Value>(&bytes)
    else {
        return bytes;
    };
    if obj.remove("oauthAccount").is_none() {
        return bytes;
    }
    serde_json::to_vec_pretty(&serde_json::Value::Object(obj)).unwrap_or(bytes)
}

fn materialize_entry(src: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Real => link_entry(src, dst),
        LinkMode::Fake => copy_tree(src, dst),
    }
}

/// Materialize the pending top-level entries into the runtime tree.
///
/// Real mode creates symlinks serially (near-free). Fake mode is a recursive
/// byte copy, so the independent top-level subtrees are fanned across a bounded
/// worker pool to cut acquire wall-time on a large `~/.claude/`. Stays inside
/// the caller's single `with_state_lock` hold — the lock is never released;
/// threads only parallelize the copy. Each subtree is disjoint (no shared dst);
/// credential reconciliation still runs serially after this returns.
fn serialize_entries(pending: &[(PathBuf, PathBuf)], mode: LinkMode) -> Result<()> {
    for (src, dst) in pending {
        materialize_entry(src, dst, mode)?;
    }
    Ok(())
}

fn materialize_entries(pending: Vec<(PathBuf, PathBuf)>, mode: LinkMode) -> Result<()> {
    if mode == LinkMode::Real || pending.len() < 2 {
        return serialize_entries(&pending, mode);
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(pending.len());
    if workers < 2 {
        return serialize_entries(&pending, mode);
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let first_err = std::sync::Mutex::new(None::<anyhow::Error>);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((src, dst)) = pending.get(idx) else {
                        break;
                    };
                    if let Err(e) = materialize_entry(src, dst, mode) {
                        let mut slot = first_err.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        break;
                    }
                }
            });
        }
    });

    match first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn reconcile_credentials(runtime_path: &Path, canonical: &Path, mode: LinkMode) -> Result<()> {
    match mode {
        LinkMode::Real => {
            sync_credentials_unlocked(runtime_path, canonical)?;
            let meta = runtime_path.symlink_metadata().ok();
            if meta.is_some_and(|m| m.file_type().is_symlink() || m.is_file()) {
                return Ok(());
            }
            if canonical.exists() {
                create_symlink(canonical, runtime_path)?;
            }
        }
        LinkMode::Fake => {
            mirror_credentials(runtime_path, canonical)?;
        }
    }
    Ok(())
}

/// Used in fake-symlink mode when the OS denies symlink creation rights.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    // `metadata` follows symlinks, unlike `symlink_metadata`: a symlink/junction
    // to a DIRECTORY in `~/.claude` (a skill linked at a plugin dir) must recurse
    // like a real dir, not hit `copy_file` — `std::fs::copy` follows the link and
    // refuses a directory. Measured, and the two platforms disagree on more than
    // wording: Windows 11 gives `PermissionDenied` / "Access is denied. (os error
    // 5)", naming a permission problem that does not exist, while Linux gives
    // `InvalidInput` with no errno ("the source path is neither a regular file
    // nor a symlink to a regular file"), since `File::open` on a directory
    // succeeds there and std refuses in `open_from` rather than at EISDIR.
    // A symlink to a FILE still reaches `copy_file`, which materializes the
    // target's bytes as a regular file, the fake-mode contract.
    let meta = src
        .metadata()
        .with_context(|| format!("failed to stat {}", src.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)
            .with_context(|| format!("failed to create {}", dst.display()))?;
        for entry in
            std::fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))?
        {
            let entry = entry?;
            // Same staging-sibling skip `union_children` makes, and for the
            // same reason: a shared fake-mode tree has a `copy_file` publish or
            // several in flight at any moment (the watchdog's `mirror_tree`
            // runs lockless), so this walk can meet a `.tmp.<pid>.<seq>` that
            // is about to be renamed away.
            //
            // Here it is worse than in the mirror, because THIS walk propagates
            // its error: on Windows the staging file is still open by the
            // publishing thread and `copy_file` fails with "used by another
            // process", which fails the whole `acquire` rather than a tick that
            // would have re-converged. Copying one would also land an orphan
            // nothing ever removes, since nothing here deletes.
            let name = entry.file_name();
            if crate::watchdog::is_staging(&name) {
                continue;
            }
            copy_tree(&entry.path(), &dst.join(name))?;
        }
        Ok(())
    } else {
        // Same publish primitive the watchdog mirror uses: a sibling session
        // shares this tree, and its lockless `mirror_tree` must never sample a
        // file this walk is still writing.
        copy_file(src, dst)
    }
}

/// One watchdog iteration. Real mode only repairs `.credentials.json` (the rest
/// is symlinks needing no maintenance). Fake mode reconciles every tree file by
/// mtime, plus the credentials file — except in isolated mode, where the tree
/// mirror is skipped so it never re-seeds the operator memory/plugins the
/// isolated runtime deliberately omits (`mirror_tree` is additive and would
/// copy `~/.claude/CLAUDE.md` back in). Credentials still reconcile.
///
/// Which member's store the credentials reconcile AGAINST is read from `swap`'s
/// cell inside the same `with_state_lock` hold that does the reconciling, so a
/// session that moved member cannot be relinked back onto the one it launched on
/// — nor have the new member's tokens written into the old member's store.
fn tick(claude_home: &Path, swap: &SessionSwap) -> Result<()> {
    let runtime = swap.runtime.as_path();
    let link = runtime.join(".credentials.json");
    match swap.mode {
        LinkMode::Real => with_state_lock(|_held| {
            sync_credentials_unlocked(&link, &swap.canonical())?;
            Ok::<_, anyhow::Error>(())
        }),
        LinkMode::Fake if swap.isolation == Isolation::Isolated => {
            with_state_lock(|_held| mirror_credentials(&link, &swap.canonical()))
        }
        LinkMode::Fake => {
            // Bulk tree walk + copies run WITHOUT the state lock: on a large
            // ~/.claude/ holding the lock across the walk stalled every
            // concurrent acquire / CLI switch for hundreds of ms per tick.
            // Lockless-safe: every per-file merge is independent, self-converging
            // under "latest mtime wins" + byte-equality skip, and never deletes
            // — a file changing in the TOCTOU window re-converges next tick.
            // mirror_tree skips settings.json / .credentials.json, so it never
            // races build_runtime_dir's per-profile writes. It CAN meet that
            // build's top-level materialize walk, since a sibling session shares
            // this tree. Publishing through `copy_file`'s rename is what keeps
            // the walk off a half-written DESTINATION; it says nothing about the
            // staging sibling itself, which is a real entry in a real directory
            // until the rename, and which both walks therefore skip by name
            // (`watchdog::is_staging`). Only credential reconciliation (must not
            // interleave with acquire/switch credential writes) stays under the
            // lock.
            mirror_tree(claude_home, runtime)?;
            with_state_lock(|_held| mirror_credentials(&link, &swap.canonical()))
        }
    }
}

/// If Claude Code's internal refresh replaced `<runtime>/.credentials.json` with
/// a regular file, copy its bytes into canonical creds and swap the file back to
/// a symlink so canonical stays the single source of truth. Returns `true` when
/// bytes were written. Real-symlink mode only — fake mode uses
/// [`mirror_credentials`].
///
/// Running this outside the state flock races the credential writes of a
/// concurrent `acquire` or switch, which is what that flock exists to serialize.
/// A [`crate::lock::StateLockHeld`] witness could be threaded here, but the unit
/// tests below drive it with no hold at all — the rank stack is the next best
/// check until those grow a `HomeSandbox`.
///
/// Ceiling: the assert is off under `cfg(test)`, because 20 inline tests drive
/// this and `mirror_credentials` as units with no home sandbox, so taking the
/// flock there would lock the operator's REAL `~/.clauth` and expose hermetic
/// tests to the 25s state-lock timeout against a live daemon. Upgrade path: give
/// those tests a `HomeSandbox`, then drop the `not(test)`.
fn sync_credentials_unlocked(link_path: &Path, canonical: &Path) -> Result<bool> {
    debug_assert!(
        cfg!(test) || crate::lockorder::holds::<crate::lockorder::rank::State>(),
        "sync_credentials_unlocked without the state flock races acquire/switch \
         credential writes"
    );
    let Ok(meta) = link_path.symlink_metadata() else {
        return Ok(false);
    };
    if meta.file_type().is_symlink() {
        return Ok(false);
    }
    let runtime_bytes = std::fs::read(link_path).context("failed to read live credentials")?;
    // Skip if CC's write is mid-flight (partial, invalid, or empty object).
    // {} deserializes as ClaudeCredentials { claude_ai_oauth: None } because
    // the field is Option — require Some to confirm a completed write.
    let Ok(runtime_creds) = serde_json::from_slice::<ClaudeCredentials>(&runtime_bytes) else {
        return Ok(false);
    };
    if runtime_creds.claude_ai_oauth.is_none() {
        return Ok(false);
    }
    let canonical_bytes = std::fs::read(canonical).ok();
    let differs = canonical_bytes.as_deref() != Some(runtime_bytes.as_slice());
    // CLA-SPLIT: a static session token never rotates, so a differing runtime
    // file is a session-side re-login — never adopt it over the token (that
    // would clobber the long-lived login with a rotating chain). Keep
    // canonical and relink; the re-login stays recoverable in the runtime
    // file's lineage, and `clauth login` is the supported way to refresh the
    // profile's usage OAuth pair.
    if differs
        && canonical
            .file_name()
            .is_some_and(|f| f == "session-token.json")
    {
        logline!(
            "clauth: watchdog kept the static session token \
             (a session-side re-login is never adopted over it)"
        );
        relink_to_canonical(link_path, canonical)?;
        return Ok(false);
    }
    let mut wrote_canonical = false;
    if differs {
        // Bytes differ. The keep-canonical-vs-adopt-runtime decision (write
        // recency primary, `expires_at` as the tie-break) lives in
        // `resolve_credential_winner` — see its doc for why mtime, not expiry,
        // is the signal.
        let canonical_exp = canonical_bytes.as_deref().and_then(|cb| {
            let c = serde_json::from_slice::<ClaudeCredentials>(cb).ok()?;
            Some(c.claude_ai_oauth?.expires_at.unwrap_or(0))
        });
        let runtime_exp = runtime_creds
            .claude_ai_oauth
            .as_ref()
            .map(|o| o.expires_at.unwrap_or(0));
        // Not the raw mtime: a swap onto this member stamps its store without
        // writing it, and the runtime side is written by Claude Code, so there is
        // no marker to attach there to compensate.
        let canonical_mtime = crate::profile_cache::effective_write_time(canonical);
        let runtime_mtime = meta.modified().ok();
        if resolve_credential_winner(canonical_exp, runtime_exp, canonical_mtime, runtime_mtime) {
            // Canonical written at/after the runtime re-login (or wins the
            // tie-break); don't overwrite it with the runtime bytes.
            logline!(
                "clauth: watchdog kept canonical credentials \
                 (canonical written more recently than runtime); \
                 not overwriting with runtime re-login bytes"
            );
        } else {
            atomic_write_600(canonical, &runtime_bytes)?;
            wrote_canonical = true;
        }
    }
    relink_to_canonical(link_path, canonical)?;
    Ok(wrote_canonical)
}

/// Decide whether to keep the canonical credentials instead of adopting the
/// runtime file's bytes, given each side's token `expires_at` and file mtime.
/// Returns `true` to keep canonical.
///
/// The two files can hold INDEPENDENT, both-valid refresh-token chains: the
/// TUI/scheduler may rotate canonical while Claude Code writes a fresh
/// interactive re-login into the runtime file. So `expires_at` is the wrong
/// primary signal — it's a property of the token, not of which login the user
/// performed last. A forced rotate-all (`t` key) can stamp a canonical token
/// whose `expires_at` is marginally later than CC's fresh login; keeping
/// canonical there would silently discard that login and burn its chain.
///
/// Primary signal is write recency (mtime): CC's `unlink+write` re-login and
/// our `atomic_write` both bump mtime, so "most recently written wins" reflects
/// the intended-live login. `expires_at` is the tie-break only when mtimes are
/// equal/unavailable, and a full tie keeps canonical. A missing/unparseable
/// canonical (`canonical_exp` = `None`) always lets runtime win.
fn resolve_credential_winner(
    canonical_exp: Option<i64>,
    runtime_exp: Option<i64>,
    canonical_mtime: Option<std::time::SystemTime>,
    runtime_mtime: Option<std::time::SystemTime>,
) -> bool {
    match (canonical_exp, runtime_exp) {
        // Canonical present and parseable: mtime is the primary signal — trust
        // the most recently written file regardless of token expiry. expires_at
        // is the tie-break only when mtimes are equal/unavailable; canonical
        // wins that fallback tie.
        (Some(ce), Some(re)) => match (canonical_mtime, runtime_mtime) {
            (Some(cm), Some(rm)) if cm != rm => cm > rm,
            _ => ce >= re,
        },
        // Runtime has no token: nothing to adopt, keep canonical.
        (Some(_), None) => true,
        // Canonical missing or unparseable: runtime always wins, never let a
        // newer mtime on corrupt/absent canonical override that.
        _ => false,
    }
}

/// Repoint the runtime credential link at canonical so canonical stays the
/// single source of truth. Swaps via a temp symlink + atomic rename so a sibling
/// session never sees the path missing; if canonical is gone, removes the file.
fn relink_to_canonical(link_path: &Path, canonical: &Path) -> Result<()> {
    if canonical.exists() {
        let tmp = link_path.with_file_name(format!(".credentials.json.tmp.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        create_symlink(canonical, &tmp)?;
        std::fs::rename(&tmp, link_path)?;
    } else {
        std::fs::remove_file(link_path)?;
    }
    Ok(())
}

/// Bidirectional mtime mirror between `runtime/.credentials.json` and canonical
/// creds: "latest mtime wins", newer side copied over older. Skips partial
/// writes (invalid JSON). Fake-symlink mode only.
fn mirror_credentials(runtime_path: &Path, canonical: &Path) -> Result<()> {
    // Same flock requirement and same test ceiling as `sync_credentials_unlocked`.
    debug_assert!(
        cfg!(test) || crate::lockorder::holds::<crate::lockorder::rank::State>(),
        "mirror_credentials without the state flock races acquire/switch \
         credential writes"
    );
    let runtime_meta = runtime_path.metadata().ok();
    let canonical_meta = canonical.metadata().ok();

    if let Some((src, dst)) = newer_side(runtime_path, canonical, runtime_meta, canonical_meta) {
        copy_if_valid_creds(src, dst)?;
    }
    Ok(())
}

/// Resolve which credential side is newer or sole-present. Returns `(src, dst)`
/// where bytes should flow from `src` to `dst`, or `None` when equal/unknown.
fn newer_side<'a>(
    runtime_path: &'a Path,
    canonical: &'a Path,
    runtime_meta: Option<std::fs::Metadata>,
    canonical_meta: Option<std::fs::Metadata>,
) -> Option<(&'a Path, &'a Path)> {
    match (runtime_meta, canonical_meta) {
        (Some(rm), Some(cm)) => match rm.modified().ok().zip(cm.modified().ok()) {
            Some((rt, ca)) if rt > ca => Some((runtime_path, canonical)),
            Some((rt, ca)) if ca > rt => Some((canonical, runtime_path)),
            _ => None,
        },
        (Some(_), None) => Some((runtime_path, canonical)),
        (None, Some(_)) => Some((canonical, runtime_path)),
        (None, None) => None,
    }
}

fn copy_if_valid_creds(src: &Path, dst: &Path) -> Result<()> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    // Same guard as sync_credentials_unlocked: reject partial, invalid, or
    // empty-object writes before letting them stomp the canonical file.
    let Ok(creds) = serde_json::from_slice::<ClaudeCredentials>(&bytes) else {
        return Ok(());
    };
    if creds.claude_ai_oauth.is_none() {
        return Ok(());
    }
    if std::fs::read(dst).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    atomic_write_600(dst, &bytes).with_context(|| format!("failed to write {}", dst.display()))
}

/// Walk both `~/.claude/` and the runtime tree; copy the newer bytes onto the
/// older, seeding one-sided files onto the other — CC may create runtime-side
/// state (project history, scratch files) and the user may add `~/.claude/`
/// entries between ticks, both must propagate. **No deletion**: a file missing
/// from one side is "not yet seen", never "intentionally removed", so the mirror
/// never destroys data. Top-level `settings.json` / `.credentials.json` are
/// skipped (settings is a rewritten copy; credentials has its own stricter
/// mirror). Fake-symlink mode only.
///
/// The walk closes with one pass over [`AliasClasses`], which is what keeps two
/// `~/.claude` names resolving to ONE file from letting sort order decide whose
/// bytes survive.
fn mirror_tree(claude_home: &Path, runtime: &Path) -> Result<()> {
    // `.claude.json` is a per-profile copy reconciled by `crate::claude_json`,
    // not part of the `~/.claude/` tree — skip it here so the tree mirror never
    // copies it into `~/.claude/.claude.json`.
    let skip_top: HashSet<&str> = ["settings.json", ".credentials.json", ".claude.json"]
        .into_iter()
        .collect();
    // The one `canonicalize` this walk pays for: every entry below inherits it.
    let root_key = claude_home.canonicalize().ok();
    let mut classes = AliasClasses::default();
    for name in union_children(claude_home, runtime) {
        if name.to_str().is_some_and(|n| skip_top.contains(n)) {
            continue;
        }
        merge_path(
            &claude_home.join(&name),
            &runtime.join(&name),
            root_key.as_deref(),
            &mut classes,
        )?;
    }
    classes.converge()
}

/// Runtime copies grouped by the canonical file a mirror write to them lands on.
///
/// Two `~/.claude` names can resolve to ONE file — `CLAUDE.local.md` symlinked at
/// `CLAUDE.md`, or two names reached through one linked directory. Real symlink
/// mode has no such split: [`build_runtime_dir_with_active_env`] links each
/// top-level entry, so both runtime names ARE that one file and the last write
/// wins. Fake mode's two independent copies are the emulation gap, and merging
/// them name by name lets `union_children`'s SORT ORDER decide which copy's bytes
/// survive — the first name publishes onto the shared target and stamps it with
/// mtime-now, which the next name then reads as a genuinely newer side. Nobody
/// made that decision, so the class converges on the CLOCK instead, which is the
/// closest fake-mode analogue of the single file real mode hands it.
///
/// Two limits, accepted rather than worked around: `canonicalize` does not
/// collapse HARD links, so two hard-linked names stay two classes; and the map is
/// per-walk, so a class first seen across a tick boundary converges on the next
/// tick rather than this one.
#[derive(Default)]
struct AliasClasses {
    classes: HashMap<PathBuf, AliasClass>,
}

/// One canonical file plus every runtime copy that aliased it during this walk.
struct AliasClass {
    /// The canonical side's clock, as the walk last DECIDED it — never a re-stat.
    /// A sibling name's publish inside this same tick stamps the target with
    /// mtime-now, and comparing against that would let it outrank every real
    /// reading still to come.
    owner_time: Option<SystemTime>,
    copies: Vec<PathBuf>,
}

impl AliasClasses {
    /// Record `copy` as an alias of `target`, seeding the class's clock from
    /// `seed` the first time the walk reaches it. Returns the clock `copy` must
    /// be compared against.
    fn observe(
        &mut self,
        target: &Path,
        copy: &Path,
        seed: Option<SystemTime>,
    ) -> Option<SystemTime> {
        let class = self
            .classes
            .entry(target.to_path_buf())
            .or_insert_with(|| AliasClass {
                owner_time: seed,
                copies: Vec::new(),
            });
        class.copies.push(copy.to_path_buf());
        class.owner_time
    }

    /// The canonical side just took `copy`'s bytes, so it now carries its clock.
    fn adopt_owner_time(&mut self, target: &Path, when: Option<SystemTime>) {
        if let Some(class) = self.classes.get_mut(target) {
            class.owner_time = when;
        }
    }

    /// Give every copy in an ALIASED class the target's bytes. The per-name merge
    /// above has already put the class's newest bytes on the target, so this is
    /// what converges a copy the walk visited BEFORE the eventual winner — within
    /// the same tick, rather than one tick per alias.
    ///
    /// It also decides one case the per-name merge deliberately declines: an
    /// exact mtime tie with divergent bytes, which `mtime_newer`'s strict `>`
    /// leaves untouched on both sides. Inside a class that would be a standing
    /// disagreement between two spellings of ONE file, which has no resting
    /// state, so the target's bytes win. Outside a class the tie is still left
    /// alone, because two independent files are allowed to differ.
    ///
    /// Single-copy classes are the whole non-aliased tree and are skipped, so it
    /// costs nothing beyond the map. An unreadable target skips its class, since
    /// there is nothing to converge onto. A failed PUBLISH still fails the tick,
    /// like every other publish in the walk — and `tick` runs `mirror_tree`
    /// before `mirror_credentials`, so an error here also costs that tick its
    /// credential reconcile.
    fn converge(&self) -> Result<()> {
        for (target, class) in &self.classes {
            if class.copies.len() < 2 {
                continue;
            }
            let Ok(bytes) = std::fs::read(target) else {
                continue;
            };
            for copy in &class.copies {
                if std::fs::read(copy).ok().as_deref() == Some(bytes.as_slice()) {
                    continue;
                }
                copy_file(target, copy)?;
            }
        }
        Ok(())
    }
}

/// Identity of the canonical file a write aimed at `a` lands on, given the
/// already-resolved identity of its parent directory and `a`'s own
/// `symlink_metadata`. Every symlink component is resolved, but each one only
/// once — a link resolves itself, and everything else inherits its parent's
/// answer and appends its own name.
///
/// Resolving each entry independently is the obvious spelling and the expensive
/// one: `canonicalize` walks the whole path per FILE rather than per directory,
/// which cost a third of the walk on a large converged tree.
///
/// Inheriting is also what gives a stable identity to a canonical path that does
/// not exist YET ([`merge_path`]'s `(None, Some(_))` arm, where `canonicalize`
/// can answer nothing), and what catches the DIRECTORY spelling of the aliasing
/// bug — two `~/.claude` names linked at one directory, reaching its files under
/// leaf names that are not themselves links.
fn child_key(
    a: &Path,
    parent_key: Option<&Path>,
    a_meta: Option<&std::fs::Metadata>,
) -> Option<PathBuf> {
    if a_meta.is_some_and(|m| m.file_type().is_symlink()) {
        return a.canonicalize().ok();
    }
    let name = a.file_name()?;
    Some(parent_key?.join(name))
}

/// Unioned child-name set of two directories, minus the publishes in flight.
/// Absent/unreadable side contributes nothing. Names sorted for deterministic,
/// stable iteration.
fn union_children(a: &Path, b: &Path) -> Vec<std::ffi::OsString> {
    let mut names: HashSet<std::ffi::OsString> = HashSet::new();
    for dir in [a, b] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                // A staging sibling belongs to a `copy_file` mid-publish — one
                // of the several a shared fake-mode tree has running at once.
                // Walking it either fails the whole tick when the source is
                // renamed away between the stat and the copy, or lands an
                // orphan on the other side that nothing ever removes, since the
                // mirror never deletes.
                if crate::watchdog::is_staging(&name) {
                    continue;
                }
                names.insert(name);
            }
        }
    }
    let mut out: Vec<_> = names.into_iter().collect();
    out.sort();
    out
}

/// Reconcile one path between canonical (`a`) and runtime (`b`) sides.
/// Directories recurse via the same union-walk; files merge by mtime.
///
/// `parent_key` is `a`'s parent directory with every symlink component already
/// resolved, threaded down so [`child_key`] costs a join instead of a walk.
/// `classes` is the walk-scoped alias bookkeeping — see [`AliasClasses`] for why
/// the mtime comparisons below read the class's clock instead of re-stating `a`.
fn merge_path(
    a: &Path,
    b: &Path,
    parent_key: Option<&Path>,
    classes: &mut AliasClasses,
) -> Result<()> {
    let a_meta = a.symlink_metadata().ok();
    let b_meta = b.symlink_metadata().ok();

    // An entry EITHER side has but nothing can follow costs this one name, never
    // the tick. Its path is occupied — `symlink_metadata` sees it — while every
    // read, write and stat through it fails, so each branch below misfires on it:
    // `files_match` and `copy_file` read through it, and the directory branch
    // sees `exists()` false and calls a recursive create that returns EEXIST
    // rather than succeeding. One moved-aside `~/.claude` target would otherwise
    // take down every reconcile pass on a copy-transport host.
    //
    // Skipped whole, never unlinked: the mirror is additive and "not yet seen"
    // is already its vocabulary for a name it cannot merge. Canonical is the
    // OPERATOR's tree, so a link there is their intent, and the runtime side's
    // self-heal belongs to `prune_dangling_links` at build time.
    //
    // Two accepted limits, both PERMANENT rather than one-tick, because the skip
    // is unconditional and mutates neither side, so nothing re-converges them:
    //
    // - a canonical dangling link with a real runtime FILE under the same name
    //   strands that file. Real symlink mode would not: a write through a
    //   dangling link creates the target and leaves the link a link (measured on
    //   Linux), so fake mode diverges here rather than emulating.
    // - a dangling canonical DIRECTORY link stalls that whole subtree, not one
    //   name.
    //
    // Plus the soft edge `prune_dangling_links` documents: `Path::exists`
    // swallows every stat error, so a live link over a dropped mount reads as
    // unresolvable and is skipped for that tick.
    if is_unresolvable_entry(a, a_meta.as_ref()) || is_unresolvable_entry(b, b_meta.as_ref()) {
        return Ok(());
    }

    // Resolved once here and handed to the recursion, so every symlink component
    // on the way down is resolved once per DIRECTORY rather than once per file.
    let key = child_key(a, parent_key, a_meta.as_ref());

    // `Path::is_dir` follows symlinks, unlike the `symlink_metadata` file-type
    // above: a symlink/junction to a DIRECTORY must recurse like a real dir, or
    // `copy_file` hits `std::fs::copy` on a directory and fails the whole tick
    // (per-platform error strings on `copy_tree`). BOTH sides, not just the
    // canonical one: under fake mode Claude Code runs out of the shared runtime
    // tree, so a plugin skill it links lands on the `b` side with nothing
    // opposite it. `a_meta`/`b_meta` stay `symlink_metadata` for the existence
    // match below.
    let a_is_dir = a.is_dir();
    let b_is_dir = b.is_dir();

    if a_is_dir || b_is_dir {
        if a_is_dir && !b.exists() {
            std::fs::create_dir_all(b)
                .with_context(|| format!("failed to create {}", b.display()))?;
        }
        if b_is_dir && !a.exists() {
            // `a` is the canonical `~/.claude/` side (see `mirror_tree`'s callers) —
            // owner-only like every other dir clauth creates there, not the
            // process umask, matching the rescue path's `mkdir_700` invariant.
            crate::profile::mkdir_700(a)
                .with_context(|| format!("failed to create {}", a.display()))?;
        }
        for name in union_children(a, b) {
            merge_path(&a.join(&name), &b.join(&name), key.as_deref(), classes)?;
        }
        return Ok(());
    }

    // A symlink to a FILE is followed on the CANONICAL side only, for both the
    // clock and the write. `a_meta`/`b_meta` stay `symlink_metadata`, because
    // the match below asks only "does this entry exist", which a dangling link
    // answers yes to.
    //
    // Canonical (`a`) is the OPERATOR's tree, so a link there is their intent
    // and both halves have to honour it. The clock: a symlink carries its own
    // mtime, and writing its target never moves it, so the link side loses every
    // comparison once the other side has been written even once and the mirror
    // copies STALE bytes back over an edit the operator just made. It also
    // disagrees with Claude Code, whose re-read gate stats THROUGH a link at the
    // target ("an mtime-preserving swap is invisible"). The write: `copy_file`
    // publishes by rename,
    // which replaces the link itself with a regular file and strands the
    // operator's real file where nothing reads it.
    //
    // Runtime (`b`) is CLAUTH's tree, built by copy, and deliberately does not
    // follow. A link there is not the operator's intent, and following one would
    // aim a mirror write at an arbitrary absolute path outside BOTH trees;
    // renaming a
    // regular file over it instead restores the copy-of-canonical shape the tree
    // is meant to have. The DIRECTORY branch above still follows both sides,
    // because there the alternative is `copy_file` on a directory, which is a
    // hard error rather than a choice.
    let a_write = write_target(a);
    let a_time = a.metadata().ok().and_then(|m| m.modified().ok());
    let b_time = b_meta.as_ref().and_then(|m| m.modified().ok());

    // The clock to judge `b` against. `(None, None)` records nothing: a name
    // neither side has must never enter a class, or `converge` would CREATE the
    // runtime copy.
    let owner_time = match (&a_meta, &b_meta) {
        (None, None) => a_time,
        _ => key
            .as_deref()
            .map_or(a_time, |k| classes.observe(k, b, a_time)),
    };

    match (a_meta, b_meta) {
        (Some(_), Some(_)) => {
            if files_match(a, b)? {
                // The class clock stays where it is. A copy byte-equal to the
                // canonical target is overwhelmingly this mirror's OWN echo from
                // an earlier tick, and an mtime move is not a write, so its
                // clock is evidence of nothing. Advancing to it lets the echo
                // outrank a sibling copy carrying a real edit, and the merge
                // then publishes the old shared bytes over that edit — which
                // `mirror_tree` promises never to do.
                return Ok(());
            }
            if mtime_newer(owner_time, b_time) {
                copy_file(a, b)?;
            } else if mtime_newer(b_time, owner_time) {
                copy_file(b, &a_write)?;
                if let Some(k) = key.as_deref() {
                    classes.adopt_owner_time(k, b_time);
                }
            }
        }
        (Some(_), None) => {
            copy_file(a, b)?;
        }
        (None, Some(_)) => {
            copy_file(b, &a_write)?;
            if let Some(k) = key.as_deref() {
                classes.adopt_owner_time(k, b_time);
            }
        }
        (None, None) => {}
    }
    Ok(())
}

/// Where a write aimed at `p` must actually land: `p` itself, or what it points
/// at when `p` is a symlink that still resolves. Answers the write question
/// only — "does this entry exist" stays `symlink_metadata`'s, and "is this a
/// directory to traverse" stays `Path::is_dir`'s.
///
/// Called on the CANONICAL side only. It hands back an absolute path that can
/// leave both trees, which is correct for a link the operator made and wrong for
/// one found in clauth's own copy; see [`merge_path`].
///
/// A link `canonicalize` cannot resolve falls back to `p` itself, so the write
/// re-creates it as a regular file.
fn write_target(p: &Path) -> PathBuf {
    match p.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => p.canonicalize().unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

/// Does `p` name an entry that exists but cannot be followed? Takes the caller's
/// already-taken `symlink_metadata` so the entry's presence and the
/// follow-through `exists()` describe one stat pair rather than two.
///
/// Deliberately not "is this a dangling symlink". Four shapes answer yes, and
/// [`merge_path`] fails identically on all four, so the predicate is written to
/// the outcome rather than to one cause: a link whose target is gone, a link that
/// loops (ELOOP), a regular file unlinked between the caller's
/// `symlink_metadata` and this `exists()`, and a parent that lost `+x` in the
/// same window. `mirror_tree` walks lockless by its own doc, so both races are
/// ordinary. `Path::exists` swallowing every stat error is what folds EACCES and
/// ELOOP in beside ENOENT.
fn is_unresolvable_entry(p: &Path, meta: Option<&std::fs::Metadata>) -> bool {
    meta.is_some() && !p.exists()
}

fn mtime_newer(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

fn files_match(a: &Path, b: &Path) -> Result<bool> {
    let a_bytes = std::fs::read(a).with_context(|| format!("failed to read {}", a.display()))?;
    let b_bytes = std::fs::read(b).with_context(|| format!("failed to read {}", b.display()))?;
    Ok(a_bytes == b_bytes)
}

/// The ONE way fake-symlink mode publishes a file: stream `src` into a uniquely
/// named hidden sibling of `dst`, then rename. Used by both the bulk materialize
/// walk ([`copy_tree`]) and the watchdog mirror ([`merge_path`]), so the two can
/// never drift on atomicity or on mode.
///
/// **Atomic.** `mirror_tree` runs lockless, so a concurrent reader — a sibling
/// session sharing this tree, the Claude Code running out of it, or
/// `build_runtime_dir`'s own walk — could observe `dst` mid-write. A raw
/// `std::fs::copy` truncates-then-streams, and `mirror_tree` is BIDIRECTIONAL and
/// mtime-wins: a half-written `dst` is byte-different with mtime-now, so
/// `merge_path` would read it as the newer side and copy the TRUNCATED bytes back
/// over `~/.claude/<entry>`. Nothing repairs that — the next tick sees two
/// matching truncated files and converges on the loss. The rename makes the swap
/// atomic on POSIX (an observer sees old or complete-new); the per-writer tmp
/// suffix keeps two threads of one process off the same staging path.
///
/// **Mode-preserving.** `std::fs::copy` carries the source's permission bits
/// over, which a read-then-`atomic_write` does not (that creates at the umask).
/// `~/.claude` holds `statusline.sh`, hooks, and plugin executables, and both
/// directions matter: a runtime copy at 0644 runs a Claude Code whose hooks fail,
/// and a write-back at 0644 strips `+x` off the operator's own file outside the
/// runtime tree.
///
/// **Streaming.** The bulk path copies a whole `~/.claude` fanned across a worker
/// pool, so reading each file whole would peak at workers × largest file.
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = crate::profile::tmp_sibling(dst);
    std::fs::copy(src, &tmp)
        .with_context(|| format!("failed to copy {} -> {}", src.display(), tmp.display()))?;
    match std::fs::rename(&tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("failed to publish {}", dst.display()))
        }
    }
}

#[cfg(unix)]
fn link_entry(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("failed to symlink {} -> {}", dst.display(), src.display()))
}

#[cfg(windows)]
fn link_entry(src: &Path, dst: &Path) -> Result<()> {
    let result = if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    };
    result.with_context(|| {
        format!(
            "failed to symlink {} -> {} (enable developer mode or run as admin)",
            dst.display(),
            src.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn link_entry(_src: &Path, _dst: &Path) -> Result<()> {
    anyhow::bail!("clauth start requires symlink support");
}

#[cfg(test)]
#[path = "../tests/inline/runtime.rs"]
mod tests;
