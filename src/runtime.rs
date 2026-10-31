//! Per-session `CLAUDE_CONFIG_DIR` trees used by `clauth start`.
//!
//! Every `clauth start <profile>` session gets its OWN runtime tree, keyed by a
//! session id (`<pid>-<seq>`):
//! `~/.clauth/profiles/<profile>/runtime-<sid>/`, or `runtime-isolated-<sid>/`
//! for an isolated run. The one exception is a SHARED session under
//! [`LinkMode::Fake`], which shares the bare-stem tree per profile (see the
//! keying rule below). Its `.credentials.json` still resolves to the profile's
//! canonical creds, so concurrent sessions of one profile observe a single
//! chain of refresh tokens. A watchdog thread in each parent process keeps the
//! runtime tree and canonical state in sync.
//!
//! The layout rests on ONE rule, which every enumeration below applies instead
//! of a hardcoded name list: a runtime dir named `runtime<rest>` pairs with the
//! sessions dir named `sessions<rest>`. It covers both flavors and also the
//! pre-per-session `runtime`/`sessions` pair an earlier release left on disk, so
//! liveness and GC reach a legacy tree with no migration step.
//!
//! Per-session keying follows the transport and the flavor. Under
//! [`LinkMode::Real`] every session gets its own `-<sid>` pair. Under
//! [`LinkMode::Fake`] an ISOLATED session gets one too, because its tree links
//! nothing from `~/.claude/`. A SHARED session under [`LinkMode::Fake`] still
//! falls back to the bare stem ([`paired_dir_names`]): its tree is a recursive
//! copy, so every session of that profile shares one tree. The accepted
//! consequence is that a fake-symlink host cannot give its SHARED sessions
//! independent credentials.
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
//! - **Fake symlinks** (symlink creation denied, or the filesystem does not
//!   support it, so `~/.clauth` on exFAT, FAT32 or SMB lands here on unix
//!   too): the runtime tree is built by recursive copy, and `.credentials.json`
//!   is a regular file. The watchdog walks both sides every tick and
//!   reconciles by "latest mtime wins" so a re-login on either side propagates
//!   to the other before another session can pick up a stale refresh token.
//!
//! Liveness lives in the paired sessions directory: the session creates the
//! marker `<sessions dir>/<sid>` and holds an exclusive `flock(2)` on it for
//! its lifetime, so any other process reads liveness without cooperation.
//! [`has_live_session`] unions every `sessions*` dir under the profile, so an
//! isolated session counts the same as a shared one, plus any Claude Code
//! background job naming a tree under the profile's dir; the destructive
//! account actions (delete, disable, rename) gate on it everywhere. Token
//! rotation gates only on macOS ([`rotation_blocked_for`]), and on the narrower
//! terms — a live marker session holding a rotatable store
//! ([`has_live_marker`] && [`live_session_holds_rotatable`]), or an UNFINISHED
//! job on one: elsewhere the session reads the
//! very credential file a rotation writes and simply follows it, while on macOS
//! its Claude Code reads a Keychain item namespaced per `CLAUDE_CONFIG_DIR` that
//! clauth cannot write. Teardown drops the marker and retires the tree: a copy
//! host's shared tree its final tick published is removed outright; any other
//! is renamed to a rescue tombstone, what only it holds lifted into
//! `~/.claude`, and removed once nothing is left in it, else kept for a later
//! sweep ([`rescue_tombstone`]). That tree is the session's own except for a
//! SHARED session under [`LinkMode::Fake`], which is retired only once the last
//! session of the profile has left; [`gc_stale_runtimes`] retires what a
//! crashed session left behind, of either flavor and in either layout, and a
//! profile's delete does the same for its own trees first
//! ([`rescue_profile_trees`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::claude::{build_claude_settings_json, create_symlink};
use crate::claude_jobs::JobTrees;
use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::{
    ClaudeCredentials, Profile, ProfileName, atomic_write_600, claude_dir, clauth_dir, home_dir,
    profile_dir, profile_subpath,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkMode {
    /// OS-level symlinks. Used on Unix normally and on Windows when the
    /// process can create symlinks (developer mode or admin).
    Real,
    /// Bidirectional mtime-based mirror. Used when symlink creation fails:
    /// privilege denial or an unsupported filesystem, on any OS.
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
/// plugins, hooks, commands, agents) or runs authenticated-but-clean. The flavor
/// decides what is materialized into the tree and, under [`LinkMode::Fake`],
/// whether the pair is keyed per session (see [`paired_dir_names`]); every
/// session shares the profile's canonical credentials and rotation lock either
/// way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Isolation {
    /// Mirror of `~/.claude/` minus the per-profile and per-supervisor state:
    /// the session behaves like the operator's.
    Shared,
    /// Credentials injected, but operator memory/plugins/hooks/commands/agents
    /// omitted and settings built from an empty base — no house style leaks.
    Isolated,
}

/// Directory-name stems. A per-session dir appends `-<sid>`; the bare stem is
/// the pre-per-session layout, and under [`LinkMode::Fake`] still the live one
/// for a shared claude session and for every codex session's sessions dir. The
/// pairing rule covers both.
const RUNTIME_STEM: &str = "runtime";
const SESSIONS_STEM: &str = "sessions";
const ISOLATED_RUNTIME_STEM: &str = "runtime-isolated";
const ISOLATED_SESSIONS_STEM: &str = "sessions-isolated";

/// The suffix naming the account's fixed BACKGROUND pair (`runtime-bg` +
/// `sessions-bg`). Shared flavour on BOTH link modes, so it never meets fake
/// mode's bare `runtime`; every holder enters the one pair and mints its own
/// `<pid>-<seq>` marker in `sessions-bg`, so the wipe and teardown rules stay
/// "no held marker and no job names the tree".
const BACKGROUND_SUFFIX: &str = "-bg";

/// A codex session home under `profiles/<name>/`. Deliberately OUTSIDE the
/// `runtime*`/`sessions*` stems: the config reconcilers walk
/// [`shared_runtime_dirs`] and would drop Claude Code's `settings.json` into
/// any dir matching those, and the GC pairing rule would collect a home it has
/// no pairing story for. A codex home named off this stem falls through both
/// untouched, which is the designed behavior, not an accident.
pub(crate) const CODEX_HOME_STEM: &str = "codex-home";

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
    /// The other flavor. Only the fake transport needs to ask: there the two
    /// collapse to separate bare homes that do NOT share one `auth.json`.
    fn other(self) -> Self {
        match self {
            Isolation::Shared => Isolation::Isolated,
            Isolation::Isolated => Isolation::Shared,
        }
    }
}

impl std::fmt::Display for Isolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Isolation::Shared => "shared",
            Isolation::Isolated => "isolated",
        })
    }
}

/// Per-process counter making each `acquire`'s [`SessionId`] unique. A single
/// process can hold several live sessions of the same profile+flavor at once —
/// the `clauth mcp` server firing overlapping `delegate`s. Keying only on the
/// pid would make the second acquire block forever on the first's `flock(2)` (an
/// exclusive lock on a second fd of the same path waits), hanging the delegate in
/// `acquire` with no session ever spawned.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Set the next minted id's sequence, posing what a recycled pid gives a later
/// process: its counter starting over at a value an older row already names.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn rewind_session_seq_for_test(seq: u64) {
    SESSION_SEQ.store(seq, Ordering::Relaxed);
}

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

    #[cfg(test)]
    fn for_test(value: &str) -> Self {
        Self(value.to_string())
    }

    /// Build from an id string already validated through [`is_session_id`].
    /// Private: only the adoption entry calls it, so no caller can conjure an
    /// id a session was never acquired under.
    fn from_validated(value: String) -> Self {
        Self(value)
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

/// True when `name` is one of the shapes clauth gives a paired dir with this
/// stem: the legacy `<stem>` and `<stem>-isolated`, the per-session
/// `<stem>-<sid>` and `<stem>-isolated-<sid>`, and the SHARED-flavour
/// background `<stem>-bg`. The single parser behind every strict name check
/// below, so the flavors and the three layouts cannot drift apart across them.
fn is_paired_dir_name(name: &str, stem: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem) else {
        return false;
    };
    // `-bg` is a shared-flavour name alone: `runtime-isolated-bg` names nothing.
    if rest == BACKGROUND_SUFFIX {
        return true;
    }
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

/// Whether `path` is a clauth CLAUDE runtime tree by position as well as name
/// — `…/profiles/<name>/runtime*`, either flavor. The claude twin of
/// [`is_codex_home_path`], for the cross-harness env hygiene a codex spawn
/// performs: an inherited `CLAUDE_CONFIG_DIR` is scrubbed only when it names
/// a tree clauth built, never the operator's own custom dir.
pub(crate) fn is_clauth_runtime_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_runtime_dir_name)
        && path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::file_name)
            == Some(std::ffi::OsStr::new("profiles"))
}

/// Sibling of [`is_runtime_dir_name`] for marker dirs.
fn is_sessions_dir_name(name: &str) -> bool {
    is_paired_dir_name(name, SESSIONS_STEM)
}

/// The suffix a rescue tombstone appends to a dead runtime dir name. A `.`
/// cannot appear in a session id ([`is_session_id`] accepts digits and one
/// `-`), so the suffixed name is rejected by [`is_paired_dir_name`]: no
/// acquire adopts it and no later GC pass re-pairs it or hands it to
/// `remove_dir_all` as a runtime.
const RESCUE_TOMBSTONE_SUFFIX: &str = ".rescuing";

/// True for the tombstone name [`gc_one_pair`], `acquire` or a teardown renames
/// a dead tree to before the out-of-lock rescue: stripping the suffix must land
/// on a runtime dir name of either flavor. The sweep that matches one finishes
/// a rescue a crash or a failed move left, under the tombstone's claim
/// ([`RescueClaim`]).
fn is_rescuing_runtime_dir_name(name: &str) -> bool {
    name.strip_suffix(RESCUE_TOMBSTONE_SUFFIX)
        .is_some_and(is_runtime_dir_name)
}

/// The runtime dir name a rescue tombstone was renamed from, or `name` itself
/// for any other name: the tree a rescue lifts still names its session while it
/// sits under the tombstone.
pub(crate) fn rescued_runtime_dir_name(name: &str) -> &str {
    if is_rescuing_runtime_dir_name(name) {
        name.strip_suffix(RESCUE_TOMBSTONE_SUFFIX).unwrap_or(name)
    } else {
        name
    }
}

/// True for a SHARED runtime dir name — a per-session `runtime-<sid>`, the
/// legacy bare `runtime`, or the fixed background `runtime-bg` — and false for
/// the isolated flavor or any unrelated name. Callers that must reach only
/// shared copies (both config reconcilers, `clauth which`) key on this rather
/// than an exact name.
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

/// Whether `name` is a codex session home (`codex-home*` under a profile dir)
/// — the codex counterpart of [`is_shared_runtime_dir_name`] for
/// session-to-profile attribution.
pub(crate) fn is_codex_home_dir_name(name: &str) -> bool {
    name.starts_with(CODEX_HOME_STEM)
}

/// Whether `path` is a codex session home by POSITION as well as name:
/// `…/profiles/<name>/codex-home*`. The positional half matters — the profile
/// charset allows a profile literally named `codex-home`, and
/// `profiles/codex-home` is a profile dir, not a home. The ONE spelling of
/// that rule: the perms sweep asks it as a bool, `which`'s codex arm extracts
/// the profile name off the same predicate.
pub(crate) fn is_codex_home_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_codex_home_dir_name)
        && path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::file_name)
            == Some(std::ffi::OsStr::new("profiles"))
}

/// The `CODEX_HOME` a child may inherit: `None` when it names a codex session
/// home clauth built, else the value itself. The one predicate both
/// [`scrub_clauth_homes`] and the store move's fallback env apply, so a scrub
/// rule change lands in both.
pub(crate) fn scrubbed_codex_home(value: Option<std::ffi::OsString>) -> Option<std::ffi::OsString> {
    value.filter(|v| !is_codex_home_path(Path::new(v)))
}

/// Drop from `command` each session home it would inherit that names a tree
/// clauth built: `CLAUDE_CONFIG_DIR` onto a runtime tree, `CODEX_HOME` onto a
/// codex home. A process spawned from inside a clauth session otherwise
/// answers as that session (`clauth which`, a `codex` run landing in another
/// profile's home) and keeps pointing at its tree after teardown. The user's
/// own custom dirs are theirs and stay.
pub(crate) fn scrub_clauth_homes(command: &mut std::process::Command) {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| is_clauth_runtime_path(Path::new(&v)))
    {
        command.env_remove("CLAUDE_CONFIG_DIR");
    }
    let codex_home = std::env::var_os("CODEX_HOME");
    if codex_home.is_some() && scrubbed_codex_home(codex_home).is_none() {
        command.env_remove("CODEX_HOME");
    }
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
/// [`LinkMode::Real`] keys every session's pair by its own `<sid>`, so sessions
/// are independent. [`LinkMode::Fake`] keys an ISOLATED session the same way:
/// [`build_runtime_dir_with_active_env`] links nothing from `~/.claude/` for it,
/// so the per-session cost is a settings file, a credentials file, and a
/// whole-file copy of `~/.claude.json` (whose size is the operator's), not a
/// tree copy. A SHARED session still lands on the bare stem:
/// that tree is built by recursive COPY of `~/.claude/`, so per-session keying
/// would charge sessions 2..N a full copy each, multiple GB apiece on a real
/// install. Disk is the whole reason for the shared fallback; the fake-mode
/// watchdog walk is NOT: `acquire` spawns one per `ProfileRuntime` either way, so
/// N sessions perform N walks per second under both keyings, and sharing only
/// converges them on one destination tree. The accepted cost is that a
/// fake-symlink host cannot give its SHARED sessions independent credentials.
///
/// `session` is a [`SessionId`]'s string: digits and one `-`, which is what
/// keeps a per-session name from spelling the `isolated` flavor stem.
fn paired_dir_names(isolation: Isolation, session: &str, mode: LinkMode) -> (String, String) {
    let suffix = match (isolation, mode) {
        (_, LinkMode::Real) | (Isolation::Isolated, LinkMode::Fake) => format!("-{session}"),
        (Isolation::Shared, LinkMode::Fake) => String::new(),
    };
    (
        format!("{}{suffix}", isolation.runtime_stem()),
        format!("{}{suffix}", isolation.sessions_stem()),
    )
}

/// The account's fixed background pair, built from the shared stems so the
/// pairing rule and the names cannot disagree.
fn background_dir_names() -> (String, String) {
    (
        format!("{RUNTIME_STEM}{BACKGROUND_SUFFIX}"),
        format!("{SESSIONS_STEM}{BACKGROUND_SUFFIX}"),
    )
}

/// The `runtime-bg` tree under `name`'s profile dir — the one tree a Claude
/// Code background job launched from this account names. A row-based call site
/// asks this when the row is a background row instead of
/// [`session_runtime_trees`], whose two arms are the per-session layouts.
pub(crate) fn background_runtime_tree(name: &ProfileName) -> Result<PathBuf> {
    let (runtime_name, _) = background_dir_names();
    profile_subpath(name, &runtime_name)
}

/// Which holder is entering the fixed background pair. The row carries the
/// difference: only the daemon's hold is exempt from the macOS rotation gate's
/// marker term ([`live_session_holds_rotatable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundHold {
    /// A `clauth start <p> -- --bg` launcher, holding the tree only for its launch.
    Launcher,
    /// The daemon, holding it while a Claude Code job names it.
    Daemon,
}

/// Which pair a [`ProfileRuntime`] occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionTree {
    /// The pair [`paired_dir_names`] resolves, keyed by the session's own id.
    PerSession,
    /// The account's fixed background pair, shared flavour on both link modes.
    Background(BackgroundHold),
}

impl SessionTree {
    fn is_background(self) -> bool {
        matches!(self, Self::Background(_))
    }
}

/// The in-lock floor's refusal for a background tree on a copy host, typed so
/// the daemon can tell a designed refusal from an ordinary adoption failure and
/// re-queue the account at most once (one re-check, so a link-mode flip is
/// adopted) instead of retrying on the backoff schedule.
#[derive(Debug)]
pub(crate) struct CopyHostBackgroundRefusal;

impl std::fmt::Display for CopyHostBackgroundRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "cannot adopt a background tree on this host: it copies runtime trees \
             instead of linking them",
        )
    }
}

impl std::error::Error for CopyHostBackgroundRefusal {}

/// The approved copy-host background refusal, the flag echoed as typed. ONE
/// source for both the `start::admit` arm and the in-lock `acquire_background`
/// launcher arm, so the two sites cannot drift.
pub(crate) fn copy_host_background_refusal(name: &str, flag: &str) -> String {
    format!(
        "'{name}': {flag} cannot run on this host, since it copies runtime trees \
         instead of linking them and a background session would outlive the \
         credentials clauth rotates (on Windows, turn on Developer Mode)"
    )
}

/// Every path one session addresses, resolved as a unit once its [`LinkMode`] is
/// known — the mode decides the dir names, so nothing here can be computed
/// before the probe.
struct SessionPaths {
    runtime: PathBuf,
    sessions: PathBuf,
    pid_file: PathBuf,
}

impl SessionPaths {
    fn resolve(
        name: &ProfileName,
        isolation: Isolation,
        session: &SessionId,
        mode: LinkMode,
    ) -> Result<Self> {
        let (runtime_name, sessions_name) = paired_dir_names(isolation, session.as_str(), mode);
        Self::at(name, session, runtime_name, sessions_name)
    }

    /// The fixed background pair. Independent of `Isolation` and [`LinkMode`]
    /// by design: the pair is shared-flavour on both, and the marker is still
    /// this holder's own `<pid>-<seq>`.
    fn resolve_background(name: &ProfileName, session: &SessionId) -> Result<Self> {
        let (runtime_name, sessions_name) = background_dir_names();
        Self::at(name, session, runtime_name, sessions_name)
    }

    fn at(
        name: &ProfileName,
        session: &SessionId,
        runtime_name: String,
        sessions_name: String,
    ) -> Result<Self> {
        let sessions = profile_subpath(name, &sessions_name)?;
        let pid_file = sessions.join(session.as_str());
        Ok(Self {
            runtime: profile_subpath(name, &runtime_name)?,
            sessions,
            pid_file,
        })
    }
}

/// The paths a holder of `tree` addresses for `session`, chosen by
/// [`SessionTree`] rather than by two call sites agreeing on it.
fn resolve_tree(
    name: &ProfileName,
    isolation: Isolation,
    tree: SessionTree,
    session: &SessionId,
    mode: LinkMode,
) -> Result<SessionPaths> {
    match tree {
        SessionTree::PerSession => SessionPaths::resolve(name, isolation, session, mode),
        SessionTree::Background(_) => SessionPaths::resolve_background(name, session),
    }
}

/// Every path at which the session a registry row names could hold its liveness
/// marker. The layout lives here so no other module rebuilds it —
/// `crate::live_sessions` tests a row's liveness through this, and would go
/// silently stale if it spelled the path itself.
///
/// A row carries the profile, the flavor, and the session id but NOT the
/// transport. For a claude session the two layouts put the marker in different
/// dirs under SHARED; an isolated session is keyed per session in both modes,
/// so those arms collapse to one path. Every candidate is deduped against the
/// ones before it as it is pushed, so a collapse costs no second `open`. Both
/// are derived from [`paired_dir_names`] and a caller treats the row as live if
/// EITHER is held — the fail-safe direction, matching
/// [`session_marker_dirs`]'s deliberately loose filter: a row reaped under a
/// live session is a live session nothing can be pointed at again, while
/// probing an absent path costs one `open` that fails, and a union reads a
/// stale spawn-settings file as live only while another session holds one of
/// its candidate paths under the same `<sid>` on the same profile.
///
/// Codex's twin does not collapse where claude's does:
/// [`codex_paired_dir_names`] drops the per-session suffix under the fake
/// transport for BOTH flavors, so an isolated codex session holds its marker in
/// the bare `sessions-isolated` dir while claude's isolated layout keeps
/// `sessions-isolated-<sid>`. Both codex arms are candidates, not one selected
/// by the row's `harness` tag: a row names no transport, and
/// [`gc_spawn_settings_files`] probes with no row at all, so no row field can
/// steer every caller.
///
/// The background pair's marker dir is a third candidate unconditionally
/// rather than behind a row flag: a background holder is a launcher, the daemon,
/// or a reload of either, and a row read from an older clauth carries no flag to
/// ask. A per-session row probing `sessions-bg/<sid>` is answered by an absent
/// file (a session id never repeats across the two layouts), and a background
/// row must be found there or GC reaps a live row.
fn session_marker_paths(
    profile: &ProfileName,
    isolated: bool,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    let isolation = if isolated {
        Isolation::Isolated
    } else {
        Isolation::Shared
    };
    let marker = |sessions_name: &str| -> Result<PathBuf> {
        Ok(profile_subpath(profile, sessions_name)?.join(session_id))
    };
    let (_, claude_real) = paired_dir_names(isolation, session_id, LinkMode::Real);
    let (_, claude_fake) = paired_dir_names(isolation, session_id, LinkMode::Fake);
    let (_, background_sessions) = background_dir_names();
    let (_, codex_real) = codex_paired_dir_names(isolation, session_id, LinkMode::Real);
    let (_, codex_fake) = codex_paired_dir_names(isolation, session_id, LinkMode::Fake);
    let mut markers: Vec<PathBuf> = Vec::new();
    for sessions_name in [
        claude_real,
        claude_fake,
        background_sessions,
        codex_real,
        codex_fake,
    ] {
        let path = marker(&sessions_name)?;
        if !markers.contains(&path) {
            markers.push(path);
        }
    }
    Ok(markers)
}

/// The two runtime trees a live-sessions row's session could use as its
/// `CLAUDE_CONFIG_DIR` — the [`LinkMode::Real`] per-session layout and the
/// [`LinkMode::Fake`] fallback — derived from [`paired_dir_names`] so no other
/// module rebuilds the layout. The bridge reads the peer's session record
/// under whichever tree holds its `sessions` dir. Both modes exist on every
/// platform (`Real` also on Windows when the process can create symlinks), so
/// this is not unix-gated.
///
/// A background row is not one of these: its tree is
/// [`background_runtime_tree`], and a caller that holds the row branches on its
/// `background` bit before asking here.
pub(crate) fn session_runtime_trees(
    profile: &ProfileName,
    isolated: bool,
    session_id: &str,
) -> Result<[PathBuf; 2]> {
    let isolation = if isolated {
        Isolation::Isolated
    } else {
        Isolation::Shared
    };
    let tree = |mode| -> Result<PathBuf> {
        let (runtime_name, _) = paired_dir_names(isolation, session_id, mode);
        profile_subpath(profile, &runtime_name)
    };
    Ok([tree(LinkMode::Real)?, tree(LinkMode::Fake)?])
}

/// The runtime tree (if any) a live-sessions row reads its `sessions/` records
/// from: the first [`session_runtime_trees`] layout holding a `sessions` dir.
/// On a fake-link box that dir is a copied tree, so the record is not under
/// `~/.claude/sessions`; the bridge and the scheduler share this pick so the
/// rule lives once.
pub(crate) fn session_sessions_tree(
    start_profile: &str,
    isolated: bool,
    session_id: &str,
) -> Option<PathBuf> {
    let profile = ProfileName::from(start_profile);
    let trees = session_runtime_trees(&profile, isolated, session_id).ok()?;
    trees
        .into_iter()
        .find(|tree| tree.join("sessions").is_dir())
}

/// The per-session runtime tree a handed-off `--with-fallback` row's job names:
/// the shared [`LinkMode::Real`] layout, since a chain opt-in only survives on
/// a host that links (a copy host refuses the launch and the executor refuses a
/// Fake tree). [`session_runtime_trees`]'s first arm is exactly that layout.
pub(crate) fn chain_session_tree(start_profile: &ProfileName, session_id: &str) -> Result<PathBuf> {
    session_runtime_trees(start_profile, false, session_id).map(|trees| trees[0].clone())
}

/// Whether the session a registry row names is still running. `true` on anything
/// the probe could not decide, keeping [`is_session_alive`]'s direction: a row
/// wrongly read as live costs one wasted registry write, while one wrongly read as
/// dead silently freezes that session out of the chain (or, for GC, reaps a live
/// session's row).
///
/// Callers choose WHICH profile to probe:
/// [`crate::live_sessions::LiveSession::marker_is_held`] probes
/// `current_member` first (where a swapped session holds its marker) and falls
/// back to `start_profile` (where a session that never moved lives), so a row
/// can never be alive for one consumer and dead for another. A caller MUST use
/// that same fallback, or GC would reap what the tally counts.
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

/// Stamp and hold the marker a background row's liveness is read from: the
/// session-id file in the account's fixed `sessions-bg/`. The twin of
/// [`hold_session_row_marker`], and here for the same reason — the layout lives
/// in this module and no test rebuilds it.
#[cfg(test)]
pub(crate) fn hold_background_row_marker(name: &ProfileName, session_id: &str) -> Result<File> {
    let (_, sessions_name) = background_dir_names();
    let dir = profile_subpath(name, &sessions_name)?;
    crate::profile::mkdir_700(&dir)?;
    let file = open_pid_file(&dir.join(session_id))?;
    file.try_lock()
        .with_context(|| format!("marker {session_id} already held"))?;
    Ok(file)
}

fn profiles_root_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles"))
}

/// The runtime trees and the profile dirs one walk of `profiles/` found.
pub(crate) struct RuntimeTrees {
    /// Every runtime tree under `profiles/`, either flavor and layout.
    pub(crate) trees: Vec<PathBuf>,
    /// Profile dirs that could not be enumerated (scoped fail-closed: only the
    /// trees under one of these read as unreadable).
    pub(crate) unreadable_profiles: Vec<PathBuf>,
}

/// Every runtime tree under `profiles/`, either flavor and layout, for the job
/// reader's tree-local store pass ([`crate::claude_jobs::job_trees`]). A
/// profile dir is enumerated the way the fail-closed census
/// ([`live_namespaced_keychain_services`]) does — `read_dir` on every entry,
/// which FOLLOWS a symlinked or junctioned profile dir — so a tree under one
/// is either read here or never collected by GC/teardown. Fail closed: a root
/// that cannot be enumerated is `Err` (a tree-local job store the walk could
/// not see must not let a sweep collect its tree); a missing root reads as no
/// trees; an unreadable PROFILE dir is scoped to that profile.
pub(crate) fn runtime_trees() -> Result<RuntimeTrees, PathBuf> {
    let root = match profiles_root_dir() {
        Ok(root) => root,
        Err(_) => return Err(PathBuf::from("~/.clauth/profiles")),
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RuntimeTrees {
                trees: Vec::new(),
                unreadable_profiles: Vec::new(),
            });
        }
        Err(_) => return Err(root),
    };
    let mut out = Vec::new();
    let mut unreadable_profiles = Vec::new();
    for profile in entries {
        let profile = match profile {
            Ok(profile) => profile,
            Err(_) => return Err(root),
        };
        let children = match std::fs::read_dir(profile.path()) {
            Ok(children) => children,
            // A plain file in `profiles/` is not a profile dir; a dir that
            // vanished mid-walk contributes nothing.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::NotADirectory =>
            {
                continue;
            }
            Err(_) => {
                unreadable_profiles.push(profile.path());
                continue;
            }
        };
        for child in children {
            let child = match child {
                Ok(child) => child,
                Err(_) => {
                    unreadable_profiles.push(profile.path());
                    break;
                }
            };
            if child.file_name().to_str().is_some_and(is_runtime_dir_name) {
                out.push(child.path());
            }
        }
    }
    Ok(RuntimeTrees {
        trees: out,
        unreadable_profiles,
    })
}

/// Every marker dir under the profile: each live session's own
/// `sessions[-isolated]-<sid>` and the bare `sessions`/`sessions-isolated`.
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

/// True iff the profile has a live session by its marker flock alone: a
/// `clauth start` session holds a flock on its marker, which
/// [`session_marker_dirs`] reads. The codex half of liveness — a codex
/// session's carrier is the operator's `auth.json` chain, never a Claude Code
/// job store, so the codex gates (delete, capture, the belt restore, the
/// standby stand-down, the codex status rows) read markers only. The claude
/// half is [`has_live_session`], which adds the job-store term.
///
/// Every unknown reads as LIVE (a marker dir that cannot be enumerated rules
/// no session out), the same fail-closed direction the union takes.
pub(crate) fn has_live_marker(name: &ProfileName) -> bool {
    match session_marker_dirs(name) {
        None => true,
        Some(dirs) => dirs
            .iter()
            .any(|dir| live_sessions_at(dir).is_none_or(|n| n > 0)),
    }
}

/// True iff the profile has at least one live session, of either flavor and in
/// ANY of its marker dirs, OR a Claude Code background job names a tree under
/// its dir, OR a handed-off chain row keeps it live. Gates the destructive
/// claude account actions (delete, disable, rename) everywhere. The macOS
/// rotation gate reads a narrower job term — UNFINISHED jobs only — through
/// [`rotation_blocked_for`], never this.
///
/// Liveness has three sources. A `clauth start` session holds a flock on its
/// marker ([`has_live_marker`]); a `clauth start <p> -- --bg` session releases
/// its marker at once while Claude Code keeps reading its config dir, so a
/// profile is also live while any job names a tree under its dir
/// ([`crate::claude_jobs::JobTrees::spares_under`]); a handed-off chain row
/// keeps every account in its visit history live, its current member and its
/// start profile, even before a daemon holds it
/// ([`handed_off_live_members`]). The job store is read only when the marker
/// probe found nothing live, so a live session costs no store walk.
///
/// Every unknown reads as LIVE: a false negative pulls an account out from
/// under a running session or signs it out, while a spurious true costs a
/// refused delete the user can retry, so fail-closed is the right direction (a
/// wrongly deleted account is unrecoverable). Only a dir that is genuinely
/// absent counts as idle.
pub(crate) fn has_live_session(name: &ProfileName) -> bool {
    has_live_marker(name) || {
        let jobs = crate::claude_jobs::job_trees();
        job_names_tree_under(name, &jobs) || handed_off_live_members(&jobs).contains(name.as_str())
    }
}

/// [`has_live_session`] against a job store the caller already read, so a
/// pass-level caller reads the store once and every profile it maps shares that
/// snapshot (the [`gc_runtime_trees`] precedent). The handed-off live-member
/// set is the same shape: computed once per pass and shared, never re-read per
/// profile. The marker probe is always a fresh read — the snapshot covers only
/// the job-store walk and the handed-off set, the expensive half, and the `||`
/// keeps that walk lazy when a marker is already live.
pub(crate) fn has_live_session_in(
    name: &ProfileName,
    jobs: &crate::claude_jobs::JobTrees,
    handed_off: &HashSet<String>,
) -> bool {
    has_live_marker(name) || job_names_tree_under(name, jobs) || handed_off.contains(name.as_str())
}

/// Every member a handed-off chain row keeps live: its visit history, the
/// member it runs on now, and the launch member. The history is the complete
/// record the swap executor writes, but a member can be named by
/// `current_member` alone — a row written before the history field existed, or
/// a field view that set the member directly — so the union never drops one.
fn row_live_members(row: &crate::live_sessions::LiveSession) -> Vec<String> {
    let mut members = row.visited_members.clone();
    if let Some(current) = &row.current_member
        && !members.contains(current)
    {
        members.push(current.clone());
    }
    if !members.contains(&row.start_profile) {
        members.push(row.start_profile.clone());
    }
    members
}

/// The members a handed-off chain row keeps live, computed once per pass over
/// the registry so a caller mapping many profiles does not re-read the whole
/// registry for each one. A handed-off row records every member the session has
/// run on (see [`row_live_members`]), and its per-session tree sits under the
/// START profile's dir, so [`job_names_tree_under`] over a swapped-onto
/// member's dir never sees it. Fail closed — an unreadable store spares (reads
/// as naming) like every other job reader, so a handed-off session's account
/// can never be deleted, renamed or disabled under it.
pub(crate) fn handed_off_live_members(jobs: &crate::claude_jobs::JobTrees) -> HashSet<String> {
    crate::live_sessions::list()
        .into_iter()
        .filter(|row| row.follows_chain && !row.isolated)
        .flat_map(|row| {
            let named = chain_session_tree(
                &ProfileName::from(row.start_profile.as_str()),
                &row.session_id,
            )
            .ok()
            .is_some_and(|tree| jobs.spares(&tree));
            if named {
                row_live_members(&row)
            } else {
                Vec::new()
            }
        })
        .collect()
}

/// A Claude Code background job naming any tree under this profile's
/// dir is a live session too: the job's `CLAUDE_CONFIG_DIR` keeps
/// reading a tree the marker flock never sees. Fail closed — an
/// unreadable store, or a profile dir that cannot be resolved,
/// counts as live.
fn job_names_tree_under(name: &ProfileName, jobs: &crate::claude_jobs::JobTrees) -> bool {
    profile_dir(name)
        .map(|dir| jobs.spares_under(&dir))
        .unwrap_or(true)
}

/// Whether a live session — marker-held and holding a rotatable store, or an
/// UNFINISHED background job on one — must block rotating this profile's chain.
/// Kept PURE (the caller reads `cfg!`) so both arms run from a Linux test, like
/// [`swap_support`].
///
/// macOS only, and NOT for the double-spend reason the pre-2026-07-26 code gave
/// — a double-spend costs one failed request, not the account. The real
/// mechanism is that clauth cannot reach the credential a `clauth start`
/// session's Claude Code actually reads. That child runs with
/// `CLAUDE_CONFIG_DIR=<runtime>`, and CC namespaces its Keychain item per config
/// dir (`Claude Code-credentials-<sha256(dir)[0:8]>`)
/// while a rotation's only Keychain write is the UNSUFFIXED
/// `Claude Code-credentials` mirror (`keychain.rs::keychain_mirror_rotation`); the
/// namespaced items are written by the session-start seed and the swap
/// executor, never by a rotation.
/// So a rotation leaves that CC holding the old refresh token; its own
/// re-read-and-compare sees its item unchanged and detects no race, and the
/// `invalid_grant` that follows passes its CAS guard — which also compares
/// against its own item — so it BLANKS the pair and signs the session out
/// mid-task.
///
/// A bare `claude` is deliberately NOT covered: it reads the unsuffixed item
/// clauth does write, so rotating it propagates normally. Its stand-in markers
/// live under [`live_bare_dir`], not the profile, so the marker probes that
/// build this predicate's verdict never see them.
///
/// This dissolves the moment [`crate::keychain`] derives the namespaced service
/// name. Every site that refuses goes
/// through [`rotation_blocked_for`], so that fix is a one-line change here and
/// nowhere else — keep it that way rather than re-deriving `cfg!(macos) &&
/// has_live_session` at a call site.
fn rotation_blocked_by_live_session(live: bool, is_macos: bool) -> bool {
    is_macos && live
}

/// Whether the credential file at `path` holds a refresh token — the only kind
/// a rotation can strand. Shared by the marker term
/// ([`live_session_holds_rotatable`], per the row's `launch_store`) and the job
/// term (per an unfinished tree's `.credentials.json`), so both narrow the same
/// way. Every unknown reads as YES (holds): a missing, unreadable, or
/// unparseable file blocks. The one readable shape that allows is a file that
/// parses with no `claudeAiOauth` block or no refresh token.
fn credential_holds_refresh_token(path: &Path) -> bool {
    crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(path)
        .map(|creds| creds.refresh_token().is_some())
        .unwrap_or(true)
}

/// Whether ANY live `clauth start` session that has run on `name` HOLDS a
/// credential that carries a refresh token — the only kind a rotation can
/// strand. The holding is read off the row's `launch_store`, which every swap
/// repoints at the member the session then reads, so the verdict follows the
/// session's current member, never the one it launched on.
///
/// [`rotation_blocked_by_live_session`] spells out why a live session blocks
/// rotation on macOS: that session's Claude Code holds the pair in a Keychain
/// item clauth cannot write, so a rotation leaves it spending a superseded
/// refresh token, and the `invalid_grant` that follows blanks its item and
/// signs the session out mid-task. Every step of that mechanism needs a refresh
/// token to attempt. A session holding a `session-token.json` sidecar has
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
/// EVERY unknown reads as rotatable: an unreadable marker dir, a marker with no
/// registry row (`acquire` tolerates a failed registration), and a row from a
/// clauth that predates `launch_store` all return `true` and refuse exactly as
/// today. The credential-file read shares [`credential_holds_refresh_token`]
/// with the job term, so an unreadable or half-written file reads as holding
/// there too, and the one readable shape that allows is a file that parses
/// with no `claudeAiOauth` block at all (`{}`). Bare `claude` sessions never
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
            let Some(row) = crate::live_sessions::get(session_id) else {
                return true;
            };
            // The daemon's own background hold is no "live session" for this
            // term: it holds the tree for as long as a Claude Code job names
            // it, and only that job's being unfinished (the job term) can
            // strand a rotation. A LAUNCHER's background row carries no such
            // flag, so its hold still counts.
            if row.background_hold {
                continue;
            }
            let Some(store) = row.launch_store else {
                return true;
            };
            if credential_holds_refresh_token(&store) {
                return true;
            }
        }
    }
    false
}

/// Live registry rows currently attributed to `profile` and holding `store`
/// (their `launch_store` names it), counted like the tally counts sessions:
/// attribution is `current_member` first, `start_profile` for a session that
/// never moved, and dead rows drop by the same liveness predicate the
/// tally/decision leg uses. Markers are never counted — a swapped session
/// retains old-member markers for life. `session`'s own row is excluded:
/// the caller adds the session whose write just landed by construction.
fn sessions_holding_store(profile: &ProfileName, store: &Path, session: &SessionId) -> usize {
    crate::live_sessions::list()
        .into_iter()
        .filter(|row| {
            let member = row.current_member.as_deref().unwrap_or(&row.start_profile);
            member == profile.as_str()
                && row.launch_store.as_deref() == Some(store)
                && row.session_id != session.as_str()
                && row.marker_is_held()
        })
        .count()
}

/// The fan-out warning decision behind the three namespaced-item install
/// sites (the start seed, its watchdog retry, and the swap Install arm).
/// `landed` is the item write's own outcome: only a write that completed
/// created a new copy of the rotating pair, so a failed one returns `None` —
/// the warning is success-shaped. `n` is the observed live count including
/// `session` itself, whose item write just landed; the exact sentence fires
/// only at `n >= 2`. The event line carries the profile name and the count,
/// never credential values.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the production call sites are the macOS item-install legs; the decision is pinned on every platform"
    )
)]
fn fanout_warning(
    landed: bool,
    profile: &ProfileName,
    store: &Path,
    session: &SessionId,
) -> Option<String> {
    if !landed {
        return None;
    }
    let n = sessions_holding_store(profile, store, session) + 1;
    (n >= 2).then(|| {
        format!(
            "clauth: warning: '{profile}' has {n} live sessions sharing one rotating login; run `clauth rolling-token {profile}` before one refresh signs the others out"
        )
    })
}

/// What blocks a macOS rotation, and therefore which refusal line the caller
/// renders. The three causes are distinct operator next-steps: retry a moment
/// (a launcher's own session), `claude stop` (a background job), or retry once
/// the named store clears (clauth cannot read it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RotationBlock {
    /// A live `clauth start` marker session holds a store with a refresh token.
    LiveSession,
    /// An unfinished Claude Code background job on a tree holding a refresh token.
    BackgroundJob,
    /// The job store could not be read, so no profile's chain can be judged.
    /// Carries the store dir the refusal names (the global store, or the
    /// account's own tree-local store).
    UnreadableJobStore(PathBuf),
}

/// [`rotation_blocked_by_live_session`] over the two things that strand a
/// rotation on macOS — what every rotation leg and both TUI pre-refusals call.
///
/// The first term is a marker session holding a rotatable store
/// ([`has_live_marker`] && [`live_session_holds_rotatable`]); the second is an
/// UNFINISHED Claude Code background job on a tree that holds a refresh token
/// ([`job_block`]: under the profile a job-store walk over
/// [`crate::claude_jobs::JobTrees::unfinished_under`], and a handed-off
/// per-launch chain tree attributed by its row's `launch_store`). A finished
/// job — `stopped`/`done`/`failed` — is not mid-task, and a job on a
/// refresh-less tree has nothing to strand, so both free the chain.
///
/// The daemon's own background hold is not a marker session here:
/// [`live_session_holds_rotatable`] skips a row whose `background_hold` is set,
/// because that hold lasts exactly as long as some job names the tree and the
/// job term decides it. A launcher's marker in `sessions-bg/` during its launch
/// is not exempt — it has no such flag.
///
/// `cfg!` is tested FIRST so both probes short-circuit away off macOS:
/// [`has_live_marker`] is a `read_dir` plus an `open` + `try_lock` per marker,
/// and the job-store walk runs only when the marker term did not already block,
/// so a Linux poll of every profile never pays either for a value the predicate
/// discards.
///
/// [`live_session_holds_rotatable`] runs only behind a live marker for the same
/// reason it used to run last: it is the more expensive probe (a registry read
/// plus a credential parse per live session), and it only ever narrows an
/// answer the marker already made `true`.
///
/// A global job-store failure blocks every profile's rotation on macOS until it
/// clears; a tree-local failure blocks only the profile whose store failed.
/// Fail-closed is deliberate, and the cause carries the store's path so the
/// refusal copy names what it could not read.
pub(crate) fn rotation_block_for(name: &ProfileName) -> Option<RotationBlock> {
    #[cfg(test)]
    {
        if let Some(forced) = ROTATION_BLOCK_CAUSE_OVERRIDE.with(|c| c.borrow().clone()) {
            return Some(forced);
        }
        if let Some(forced) = ROTATION_BLOCKED_OVERRIDE.with(std::cell::Cell::get) {
            return forced.then_some(RotationBlock::LiveSession);
        }
    }
    rotation_block_on(name, cfg!(target_os = "macos"))
}

pub(crate) fn rotation_blocked_for(name: &ProfileName) -> bool {
    rotation_block_for(name).is_some()
}

/// The job term: an UNFINISHED Claude Code background job on a tree that holds
/// a refresh token. The same exemption the marker term gets, applied per tree:
/// each unfinished tree under the profile's dir has its `.credentials.json`
/// read through [`credential_holds_refresh_token`], so a job on a
/// `session-token.json` sidecar (linked or copied) frees the chain exactly like
/// a marker session on one. A tree the read cannot judge (no file, a dangling
/// link, unparseable bytes) holds. Fail closed: an unreadable job store, or a
/// profile dir that cannot be resolved, blocks with the store's path named.
///
/// The under-the-profile arm cannot see a handed-off per-launch chain tree
/// correctly: the tree sits under its START profile's dir while the session
/// runs on the member its row names, so a swap off the start profile leaves
/// the tree under an account the session no longer reads. The per-launch arm
/// therefore attributes such a tree by its row's `launch_store` — the store
/// the session actually reads (on macOS it moves only once the swap's keychain
/// legs land) — else the tree's link, and blocks `name` only when that store
/// is one of `name`'s own. A store that is absent, or that no longer resolves,
/// holds for the accounts the row names and for those only: the member the
/// session runs on, the account it started on, and the account whose profile
/// dir the unresolved path points into — the only accounts a rotation of which
/// could strand it. Any other account's rotation cannot reach this session, so
/// the gate leaves it to the rest of the terms. The under-profile arm skips
/// trees a chain row owns so the launch account the session left is never
/// blocked by its own dir.
fn job_block(name: &ProfileName) -> Option<RotationBlock> {
    let jobs = crate::claude_jobs::job_trees();
    let dir = match profile_dir(name) {
        Ok(dir) => dir,
        Err(_) => {
            return Some(RotationBlock::UnreadableJobStore(PathBuf::from(
                "~/.claude/jobs",
            )));
        }
    };
    let trees = match jobs.unfinished_under(&dir) {
        Ok(trees) => trees,
        Err(store) => return Some(RotationBlock::UnreadableJobStore(store)),
    };
    let rows = crate::live_sessions::list();
    let chain_trees: HashSet<PathBuf> = rows
        .iter()
        .filter(|row| row.follows_chain && !row.isolated)
        .filter_map(|row| {
            chain_session_tree(
                &ProfileName::from(row.start_profile.as_str()),
                &row.session_id,
            )
            .ok()
            .and_then(|tree| crate::canonical::identity(&tree).ok())
        })
        .collect();
    if trees.iter().any(|tree| {
        !chain_trees.contains(tree)
            && credential_holds_refresh_token(&tree.join(".credentials.json"))
    }) {
        return Some(RotationBlock::BackgroundJob);
    }
    for row in rows {
        if !row.follows_chain || row.isolated {
            continue;
        }
        let Ok(tree) = chain_session_tree(
            &ProfileName::from(row.start_profile.as_str()),
            &row.session_id,
        ) else {
            continue;
        };
        if !jobs.unfinished_names(&tree) {
            continue;
        }
        // The store the session reads: the row's `launch_store`, else the
        // tree's credential link. A row with neither, or whose store no longer
        // resolves, holds for the accounts its row names and no others.
        let store = row
            .launch_store
            .clone()
            .or_else(|| crate::canonical::identity(tree.join(".credentials.json")).ok());
        let Some(store) = store else {
            if row_names_account(&row, name, None) {
                return Some(RotationBlock::BackgroundJob);
            }
            continue;
        };
        let Some(own) = store_is_own_of(name, &store) else {
            if row_names_account(&row, name, Some(&store)) {
                return Some(RotationBlock::BackgroundJob);
            }
            continue;
        };
        if own && credential_holds_refresh_token(&store) {
            return Some(RotationBlock::BackgroundJob);
        }
    }
    None
}

/// Whether a chain row names `name` among the accounts its unresolved store
/// can strand: the member the session runs on (`current_member`, else
/// `start_profile`), the account it started on (its tree lives under that
/// profile's dir), and — when the row names a store path — the account whose
/// profile dir holds that path. A rotation of any other account cannot reach
/// this session, so the gate leaves it to the rest of its terms rather than
/// refusing a rotation nothing here could strand.
///
/// The dirs compare canonicalized, like [`store_is_own_of`], so a via→real
/// spelling never compares unequal; the raw spelling decides when either side
/// cannot canonicalize.
fn row_names_account(
    row: &crate::live_sessions::LiveSession,
    name: &ProfileName,
    store: Option<&Path>,
) -> bool {
    if row.start_profile == name.as_str()
        || row.current_member.as_deref().unwrap_or(&row.start_profile) == name.as_str()
    {
        return true;
    }
    let Some(parent) = store.and_then(Path::parent) else {
        return false;
    };
    let Ok(dir) = profile_dir(name) else {
        return false;
    };
    if let (Ok(real_parent), Ok(real_dir)) = (
        crate::canonical::identity(parent),
        crate::canonical::identity(&dir),
    ) {
        return real_parent == real_dir;
    }
    parent == dir.as_path()
}

/// Whether `store` is one of `name`'s own credential stores — the two files a
/// switch installs from, whichever exists — compared canonicalized like the
/// adoption's own-stores check, so a via→real spelling never compares unequal.
/// `None` when `store` does not resolve (deleted, a dangling link): an identity
/// the gate cannot judge holds.
fn store_is_own_of(name: &ProfileName, store: &Path) -> Option<bool> {
    let member_dir = profile_dir(name).ok()?;
    let store = crate::canonical::identity(store).ok()?;
    Some(
        [
            member_dir.join("credentials.json"),
            member_dir.join("session-token.json"),
        ]
        .into_iter()
        .filter_map(|own| crate::canonical::identity(own).ok())
        .any(|own| own == store),
    )
}

/// [`rotation_block_for`] with the host passed in, so a Linux test drives the
/// macOS arm against real marker and job-store state.
pub(crate) fn rotation_block_on(name: &ProfileName, is_macos: bool) -> Option<RotationBlock> {
    if !is_macos {
        return None;
    }
    // An unreadable profile dir must be the unreadable-store cause, not the
    // marker term's: the marker probe reads the same dir and fails closed to
    // "live", but it cannot actually see a session there, and the ruling names
    // the unreadable path. The job term below would return the same answer;
    // this early return keeps the marker-first order for the readable case.
    if let Some(store) = unreadable_profile_dir(name) {
        return Some(RotationBlock::UnreadableJobStore(store));
    }
    if rotation_blocked_by_live_session(
        has_live_marker(name) && live_session_holds_rotatable(name),
        true,
    ) {
        return Some(RotationBlock::LiveSession);
    }
    job_block(name)
}

/// `Some(profile dir)` when it exists but cannot be enumerated: the one case
/// the marker term cannot judge — it can neither rule a session out nor claim
/// one in — so the rotation refusal names the dir as the unreadable store
/// (the refusal names the unreadable path). `None` when the dir is absent
/// (idle) or readable.
fn unreadable_profile_dir(name: &ProfileName) -> Option<PathBuf> {
    let profile = profile_dir(name).ok()?;
    match std::fs::read_dir(&profile) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(profile),
        Ok(_) => None,
    }
}

/// [`rotation_block_on`]'s yes/no form, for tests that pose the host themselves.
#[cfg(test)]
pub(crate) fn rotation_blocked_on(name: &ProfileName, is_macos: bool) -> bool {
    rotation_block_on(name, is_macos).is_some()
}

// Test seam posing the refusal's answer from a Linux host. Thread-local, same
// shape as `ROTATION_LOCK_TIMEOUT_OVERRIDE`: a test that forces it affects only
// the thread it drives the fetch on, and `None` (the default) is the host
// answer — production never sets it.
#[cfg(test)]
thread_local! {
    static ROTATION_BLOCKED_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Set or clear the test-only refusal override. `None` restores the host answer.
#[cfg(test)]
pub(crate) fn set_rotation_blocked_override(forced: Option<bool>) {
    ROTATION_BLOCKED_OVERRIDE.with(|c| c.set(forced));
}

// Test seam posing WHICH cause blocks, so the three refusal lines' rendering
// paths are drivable from a Linux host. The bool seam above keeps its arms on
// [`RotationBlock::LiveSession`]; this one covers the two job-store causes the
// host can only reach on macOS.
#[cfg(test)]
thread_local! {
    static ROTATION_BLOCK_CAUSE_OVERRIDE: std::cell::RefCell<Option<RotationBlock>> =
        const { std::cell::RefCell::new(None) };
}

/// Set or clear the test-only cause override. `None` restores the host answer.
#[cfg(test)]
pub(crate) fn set_rotation_block_override(forced: Option<RotationBlock>) {
    ROTATION_BLOCK_CAUSE_OVERRIDE.with(|c| *c.borrow_mut() = forced);
}

/// Count of live `clauth start` sessions for the profile: the flock-held markers
/// across every marker dir, one per session. Reports 1 on an unknown, so it never
/// contradicts [`has_live_session`] within a tick.
///
/// TEST-ONLY since 2026-07-25. Its one production consumer, the Services tab's
/// fleet tally, moved to `live_sessions::LiveTally`: this counts markers per
/// profile, so a session that swapped A→B reads as one session on each of two
/// accounts, and only the registry can tell those apart.
///
/// Kept rather than deleted because the marker-layout tests need the COUNT, and
/// [`has_live_session`] cannot supply it — it is a boolean `.any()`, so it reads
/// one session and two identically, which is exactly the distinction phase 0b's
/// two-sessions-on-one-profile keying rests on. Same shape as
/// [`hold_session_row_marker`]: a test-only observation of a layout this module
/// owns, so nothing outside it rebuilds the paths.
#[cfg(test)]
pub(crate) fn live_session_count(name: &ProfileName) -> usize {
    session_marker_dirs(name)
        .and_then(|dirs| dirs.iter().map(|dir| live_sessions_at(dir)).sum())
        .unwrap_or(1)
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
/// `SESSIONS_STEM`, so nothing here can reach the marker-scanning gates —
/// [`has_live_session`] (delete, disable, rename) and the rotation gate's
/// marker term — which all read a live session's marker under the profile. A
/// bare session holds no credential clauth handed it and none
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
/// and the one caller that exists leaves the fleet total unread (see
/// [`crate::live_sessions::LiveTally::collect`]).
///
/// This is a `clauth mcp` count STANDING IN for a bare `claude` count, and the
/// approximation is loose in both directions. Under: a `claude` with no clauth
/// MCP server wired boots none, so it is counted nowhere. Over: any
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
    // macOS: one shared subprocess budget spans every Keychain item delete the
    // tree sweep performs below — the daemon-tick shape, not a fresh budget per
    // pair: a sweep collecting many stale trees must not multiply a stuck
    // keychain's per-call ceiling across them on the `clauth start` / MCP-boot
    // paths this runs on. Arm-if-not-armed, so a caller that already armed a
    // wider one is adopted rather than replaced.
    #[cfg(target_os = "macos")]
    let _item_budget = crate::lock::SharedSubprocessBudget::arm(crate::lock::SUBPROCESS_BUDGET);
    // The Services tab's boot probe kills its `clauth mcp` child within 3 s, so
    // the tree sweep skips there WHOLE: on macOS it spends `security`
    // subprocesses collecting the removed trees' Keychain items, and on every
    // platform it takes the state flock per pair against the 25 s deadline a
    // macOS switch legitimately holds (the same probe-budget rule
    // `gc_bare_markers`' peek encodes). Gating the TREE half is what keeps
    // tree and item paired — skipping only the item delete would make the
    // probe a stranding producer, since the service is a one-way hash of the
    // dir and no later walk explains an item whose tree the probe removed.
    // The row/marker/record siblings still run: the split exists precisely so
    // one sweep's skip is nobody else's. The next real sweep — any start,
    // resume, TUI launch, mcp serve or daemon startup — collects both halves.
    if std::env::var_os(crate::mcp::MCP_PROBE_ENV).is_none() {
        gc_runtime_trees();
    }
    gc_live_session_rows();
    gc_bare_markers();
    gc_spawn_settings_files();
    crate::hook_note::gc_conversation_records();
    gc_codex_homes();
}

/// True for a PER-SESSION codex home name — `codex-home-<sid>` /
/// `codex-home-isolated-<sid>` — the strict form GC may `remove_dir_all`.
/// The bare stems (the durable store, and fake mode's shared home) are
/// deliberately NOT matched: `rest.is_empty()` is excluded, unlike
/// [`is_paired_dir_name`], because the bare codex home holds state that must
/// outlive every session.
fn is_per_session_codex_home_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(CODEX_HOME_STEM) else {
        return false;
    };
    let rest = rest.strip_prefix("-isolated").unwrap_or(rest);
    rest.strip_prefix('-').is_some_and(is_session_id)
}

/// Collect codex session homes whose session died without teardown. The
/// claude GC's pairing rule cannot see these (a codex home matches no
/// `runtime*` stem — by design, so the reconcilers never touch it), and the
/// marker-dir orphan branch reaps the SESSIONS dir on its own — which would
/// leave the home tree unpairable and immortal. Same liveness rule as every
/// other sweep: a dir whose paired marker dir holds a live flock is spared,
/// and so is one whose liveness cannot be READ (unknown reads as live).
fn gc_codex_homes() {
    let Ok(root) = profiles_root_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut candidates: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
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
            if !is_per_session_codex_home_name(child_name) {
                continue;
            }
            // `codex-home<rest>` pairs with `sessions<rest>` — the same
            // `<rest>` convention the runtime pairing uses. A missing marker
            // dir reads as DEAD, not live: the acquire creates the marker
            // before the home, so a home with no marker dir is a crashed
            // teardown's leftover, and sparing it would be the immortality
            // this sweep exists to end.
            let Some(rest) = child_name.strip_prefix(CODEX_HOME_STEM) else {
                continue;
            };
            candidates.push((child.path(), profile.join(format!("{SESSIONS_STEM}{rest}"))));
        }
    }
    // Peek before locking, like `gc_bare_markers`: nothing to collect must not
    // pay the state flock's wait, which a macOS switch legitimately holds for
    // ~20s. A home that appears after the peek is the next sweep's, which is
    // the whole contract of a best-effort GC.
    if candidates.is_empty() {
        return;
    }
    let _ = with_state_lock(|_held| {
        for (home, sessions) in candidates {
            // The liveness read and the removal BOTH belong under the state
            // lock, because the acquire's whole critical section runs under it
            // and the marker's flock is taken AFTER the home is built. Read
            // unlocked, the entire `build_codex_home` window shows a home
            // present beside a marker dir that is empty — indistinguishable
            // from a crash's leftover — and this sweep would reap a session
            // that is starting. Under the lock the only two states left are
            // "not begun" (no home) and "finished" (flock held).
            let dead = if sessions.exists() {
                matches!(live_sessions_at(&sessions), Some(0))
            } else {
                true
            };
            if dead {
                let _ = std::fs::remove_dir_all(&home);
                let _ = std::fs::remove_dir(&sessions);
            }
        }
        Ok::<_, anyhow::Error>(())
    });
}

fn gc_runtime_trees() {
    // One snapshot of Claude Code's job store per sweep, before the pair loop,
    // so the fast path never pays the walk per pair. A dead pair the snapshot
    // did not name pays one more store read, under the state flock (the
    // `gc_one_pair_synced` re-read). An unreadable store spares every dead pair
    // this sweep — the fail-closed direction `is_session_alive` takes — and the
    // log names the offending job dir, never its contents.
    let job_trees = crate::claude_jobs::job_trees();
    if let JobTrees::Unreadable { offending, .. } = &job_trees {
        logline!(
            "clauth: spared every dead runtime tree this sweep: a Claude Code \
             background-job record under {} could not be read",
            offending.display()
        );
    }
    // One line per scoped unreadable (a profile dir, or a tree's own `jobs/`),
    // once per sweep, naming the store the operator can act on — the per-pair
    // line stays silent for these, since no job was shown to name the tree.
    for store in job_trees.scoped_unreadable_stores() {
        logline!(
            "clauth: spared runtime trees this sweep: the Claude Code \
             background-job store {} could not be read",
            store.display()
        );
    }
    let Ok(root) = profiles_root_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let _rescue_wait = crate::sessions::RescueLockBudget::arm();
    for profile in entries.flatten() {
        gc_profile_trees(&profile.path(), &job_trees);
    }
}

/// One profile's half of [`gc_runtime_trees`]: every kept rescue tombstone
/// first, then every dead runtime tree, retired and rescued. Tombstones go
/// first because a kept `runtime.rescuing` blocks the rename that would retire
/// a dead `runtime` beside it.
fn gc_profile_trees(profile: &Path, job_trees: &JobTrees) {
    // Strict predicates, not the loose pairing split: these walks hand
    // `remove_dir_all` whatever they match, so a future `runtime_state` or
    // `sessions.json` under a profile must fall through untouched.
    for child_name in child_names(profile) {
        if is_rescuing_runtime_dir_name(&child_name) {
            rescue_tombstone(&profile.join(&child_name));
        } else if let Some(tombstone) = child_name.strip_suffix(RESCUE_CLAIM_SUFFIX)
            && is_rescuing_runtime_dir_name(tombstone)
            && profile
                .join(tombstone)
                .symlink_metadata()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            // A crash between a tombstone's removal and its claim's leaves the
            // claim alone; claiming it finds no tombstone and removes it.
            rescue_tombstone(&profile.join(tombstone));
        }
    }
    for child_name in child_names(profile) {
        if is_runtime_dir_name(&child_name)
            && let Some(sessions) = paired_sessions_name(&child_name)
        {
            let _ = gc_one_pair(
                &profile.join(&child_name),
                &profile.join(sessions),
                job_trees,
            );
        } else if is_sessions_dir_name(&child_name)
            && let Some(runtime) = paired_runtime_name(&child_name)
        {
            // A marker dir with no runtime sibling. `acquire` mints it before
            // it builds the tree, so a crash in that window strands one — and
            // per-session keying makes that a fresh empty dir every time
            // rather than a reused one.
            let runtime = profile.join(runtime);
            if runtime.symlink_metadata().is_err() {
                let _ = gc_one_pair(&runtime, &profile.join(&child_name), job_trees);
            }
        }
    }
}

/// The UTF-8 entry names under `dir`; an unreadable dir has none.
fn child_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).map_or_else(
        |_| Vec::new(),
        |children| {
            children
                .flatten()
                .filter_map(|child| child.file_name().into_string().ok())
                .collect()
        },
    )
}

/// Rescue what a profile's dead runtime trees alone hold, ahead of the
/// profile's deletion: [`gc_profile_trees`] over this profile alone, with no
/// state flock held. Returns the first runtime tree or tombstone (by name)
/// still standing under `profile`: a tombstone whose rescue left files, one
/// another sweep is finishing, or a tree the walk spared. An unreadable
/// profile dir is returned itself, since nothing shows it holds nothing.
pub(crate) fn rescue_profile_trees(profile: &Path) -> Option<PathBuf> {
    // macOS: the walk collects one Keychain item per dead tree, so it arms
    // one shared subprocess budget over all of them, as `gc_stale_runtimes`
    // does over its sweep: a stuck keychain costs a delete one per-call
    // ceiling, never one per tree.
    #[cfg(target_os = "macos")]
    let _item_budget = crate::lock::SharedSubprocessBudget::arm(crate::lock::SUBPROCESS_BUDGET);
    let _rescue_wait = crate::sessions::RescueLockBudget::arm();
    gc_profile_trees(profile, &crate::claude_jobs::job_trees());
    let children = match std::fs::read_dir(profile) {
        Ok(children) => children,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(profile.to_path_buf()),
    };
    children
        .flatten()
        .filter(|child| {
            child
                .file_name()
                .to_str()
                .is_some_and(|name| is_runtime_dir_name(name) || is_rescuing_runtime_dir_name(name))
        })
        .map(|child| child.path())
        .min()
}

/// Every namespaced Keychain service an EXISTING runtime dir explains — the
/// live set the macOS census (`keychain::census_namespaced_items`) spares.
/// Derived through the same naming rule the session seed writes under
/// (`claude::namespaced_keychain_service`, over the canonicalized dir), from
/// the same universe the tree sweep walks: every `runtime*` dir under
/// `profiles/`. A dir the sweep has not collected yet is live here; one it
/// collects on THIS pass loses its item to the sweep's own collector, and one
/// it cannot collect keeps both. Sessions dirs and unrelated names contribute
/// nothing — no CC config dir exists there to derive a service from.
///
/// FAIL-CLOSED: an enumeration or canonicalize failure that is not a dir
/// vanishing mid-walk is an error, and the census reads an underivable live
/// set as "cannot rule out a live session" — it deletes nothing. An unreadable
/// root or profile must never shrink the set to empty and hand the census a
/// delete-everything pass (the same "unknown reads live" rule the sweep's
/// marker enumeration follows). A dir that vanished between the read and the
/// canonicalize (`NotFound`) contributes nothing instead: its item is an
/// orphan and correctly collectible.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS Keychain census; the derivation is pinned on every platform"
    )
)]
pub(crate) fn live_namespaced_keychain_services() -> Result<BTreeSet<String>> {
    let mut live = BTreeSet::new();
    let root = profiles_root_dir()?;
    let entries = std::fs::read_dir(&root)
        .with_context(|| format!("cannot enumerate the profiles dir {}", root.display()))?;
    for profile in entries {
        let profile =
            profile.with_context(|| format!("cannot read an entry under {}", root.display()))?;
        let children = std::fs::read_dir(profile.path())
            .with_context(|| format!("cannot enumerate {}", profile.path().display()))?;
        for child in children {
            let child = child.with_context(|| {
                format!("cannot read an entry under {}", profile.path().display())
            })?;
            let file_name = child.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !is_runtime_dir_name(name) {
                continue;
            }
            match crate::canonical::identity(child.path()) {
                Ok(canonical) => {
                    live.insert(crate::claude::namespaced_keychain_service(&canonical));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("cannot canonicalize {}", child.path().display())
                    });
                }
            }
        }
    }
    Ok(live)
}

/// The census's per-delete gate: ONE state-lock hold re-derives the live set
/// ([`live_namespaced_keychain_services`]) and stamps the in-flight record
/// when the service is still orphaned. A seed serialized after the hold is
/// refused by the record; one serialized before it was spared by the walk —
/// its acquire already rebuilt the dir — which is what closes the residual
/// window a re-derivation running before the mint and a delete landing after
/// the seed would otherwise leave open. `None` spares the item (live since
/// the dump); `Some` is the witness the delete sink requires. macOS-only
/// caller, pinned on every platform.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS Keychain census; the flock-held gate is pinned on every platform"
    )
)]
pub(crate) fn census_delete_gate(
    service: &str,
) -> Result<Option<namespaced_keychain_ledger::InFlightDelete>> {
    with_state_lock(|_held| {
        if live_namespaced_keychain_services()?.contains(service) {
            return Ok(None);
        }
        Ok(Some(namespaced_keychain_ledger::record_in_flight_locked(
            service,
        )?))
    })
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the ownership ledger is consumed by macOS Keychain writes and census; its persistence and decisions are pinned on every platform"
    )
)]
pub(crate) mod namespaced_keychain_ledger {
    use super::*;

    const PATH: &str = "keychain-item-owners.json";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct Owner {
        pub(crate) service: String,
        pub(crate) profile: ProfileName,
        pub(crate) session: String,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub(crate) struct Owners {
        pub(crate) owners: Vec<Owner>,
    }

    pub(crate) fn path() -> Result<PathBuf> {
        Ok(clauth_dir()?.join(PATH))
    }

    pub(crate) fn load() -> Result<Owners> {
        let path = path()?;
        let owners = match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str::<Owners>(&body).map_err(|e| {
                crate::format::parse_error(path.display(), crate::format::json_parse_position(&e))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Owners::default()),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        // The ledger is an external boundary: what a clauth writer records is
        // always shape-valid, so anything outside the shapes clauth itself
        // mints is a hand edit or corruption and must fail the WHOLE ledger
        // closed — an invalid row can never mint delete authority, and neither
        // can its valid siblings.
        for owner in &owners.owners {
            if !crate::claude::is_namespaced_keychain_service(&owner.service) {
                anyhow::bail!(
                    "the namespaced Keychain ownership ledger names a service the naming rule \
                     could not produce (`{}`). Refusing to authorize any delete",
                    owner.service
                );
            }
            if let Err(e) = crate::actions::validate_name_chars(owner.profile.as_str()) {
                anyhow::bail!(
                    "the namespaced Keychain ownership ledger holds an invalid profile name \
                     (`{}`): {e}. Refusing to authorize any delete",
                    owner.profile
                );
            }
            if !is_session_id(&owner.session) {
                anyhow::bail!(
                    "the namespaced Keychain ownership ledger holds a session id outside the \
                     minted shape (`{}`). Refusing to authorize any delete",
                    owner.session
                );
            }
        }
        Ok(owners)
    }

    pub(crate) fn save(owners: &Owners) -> Result<()> {
        let path = path()?;
        if let Some(parent) = path.parent() {
            crate::profile::mkdir_700(parent)
                .context("failed to create the namespaced Keychain ownership ledger directory")?;
            crate::profile::enforce_clauth_perms(parent);
        }
        let bytes = serde_json::to_vec_pretty(owners)
            .context("failed to serialize the namespaced Keychain ownership ledger")?;
        atomic_write_600(&path, bytes)
            .context("failed to persist the namespaced Keychain ownership ledger")
    }

    pub(crate) fn owned_services() -> Result<BTreeSet<String>> {
        Ok(load()?
            .owners
            .into_iter()
            .map(|owner| owner.service)
            .collect())
    }

    pub(crate) fn record_with(
        service: &str,
        profile: &ProfileName,
        session: &SessionId,
        persist: impl FnOnce(&Owners) -> Result<()>,
    ) -> Result<()> {
        with_state_lock(|_held| {
            // The in-flight consult precedes the ownership row's persist, in
            // the same hold: a sweep whose re-check passed stamps its record
            // before its delete runs, and this refusal is what keeps that
            // delete from catching the item this call is about to authorize.
            refuse_while_in_flight_locked(service)?;
            let mut owners = load()?;
            if let Some(owner) = owners
                .owners
                .iter_mut()
                .find(|owner| owner.service == service)
            {
                owner.profile = profile.clone();
                owner.session = session.as_str().to_string();
            } else {
                owners.owners.push(Owner {
                    service: service.to_string(),
                    profile: profile.clone(),
                    session: session.as_str().to_string(),
                });
            }
            persist(&owners)
        })
    }

    pub(crate) fn retire(service: &str) -> Result<()> {
        with_state_lock(|_held| {
            let mut owners = load()?;
            let before = owners.owners.len();
            owners.owners.retain(|owner| owner.service != service);
            if owners.owners.len() != before {
                save(&owners)?;
            }
            Ok(())
        })
    }

    /// How old a PIDLESS or pid-dead in-flight stamp must be before the
    /// seed's consult sweeps it as crash-stale. The pid rule outranks age: a
    /// row whose stamped `security` child is alive refuses the write however
    /// old the row is — the deadline that would kill a stuck child is
    /// parent-local (`keychain::run_with_deadline` kills its own child), so
    /// a crashed sweeper's orphaned child outlives the bound and age alone
    /// must never admit a seed the late delete could destroy. For rows
    /// without a live child (none was ever stamped, or it exited) the bound
    /// is the guarded delete's own worst-case wall duration: two `security`
    /// invocations — the salvage read, then the delete — each capped at
    /// `keychain::SECURITY_TIMEOUT` and sharing the one
    /// [`crate::lock::SUBPROCESS_BUDGET`] both collectors arm. The tie is a
    /// compile-time assert beside `SECURITY_TIMEOUT`, on the one platform the
    /// number exists.
    pub(crate) const IN_FLIGHT_STALE_AFTER: Duration = crate::lock::SUBPROCESS_BUDGET;

    const IN_FLIGHT_PATH: &str = "keychain-deletes-in-flight.json";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct InFlightDeleteRow {
        pub(crate) service: String,
        /// Wall-clock stamp, seconds since the UNIX epoch. A future stamp
        /// reads as fresh — the fail-closed arm — until the clock passes it.
        pub(crate) stamp_secs: u64,
        /// The spawned `security` children's pids, stamped at spawn once each
        /// child exists — the service stamp precedes the spawn, so a row
        /// starts with none and gains each pid before its leg waits on the
        /// child. One row per service; concurrent collectors on the same
        /// orphaned service share it, and a collector's clear removes only
        /// its own pids.
        #[serde(default)]
        pub(crate) pids: Vec<u32>,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub(crate) struct InFlightDeletes {
        pub(crate) deletes: Vec<InFlightDeleteRow>,
    }

    pub(crate) fn in_flight_path() -> Result<PathBuf> {
        Ok(clauth_dir()?.join(IN_FLIGHT_PATH))
    }

    /// Seconds since the UNIX epoch; 0 on a pre-epoch clock, which reads every
    /// record as fresh — the fail-closed arm (a broken clock refuses writes
    /// rather than reopening the window).
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Whether a pid names a running process — `kill(pid, 0)` semantics on
    /// unix: the probe succeeds when the process exists, and `EPERM` (a
    /// process this user may not signal) still proves existence. False on
    /// non-unix, where no guarded delete ever runs and every row falls back
    /// to the age bound.
    #[allow(unsafe_code)]
    pub(crate) fn pid_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // SAFETY: `pid` is a plain integer the OS interprets as a process
            // id; signal 0 probes existence and sends nothing (the same shape
            // `start::forward_signal` uses).
            let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
            result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }

    /// Remove rows whose guarded delete can no longer be running: a row with
    /// a live stamped child stays however old it is — a crashed sweeper's
    /// orphaned `security` child outlives the age bound, since the deadline
    /// that would kill it died with the parent — and a row with no live
    /// child falls back to [`IN_FLIGHT_STALE_AFTER`]. Says whether the file
    /// needs a save.
    fn sweep_stale(deletes: &mut InFlightDeletes) -> bool {
        let now = now_secs();
        let before = deletes.deletes.len();
        deletes.deletes.retain(|row| {
            row.pids.iter().any(|pid| pid_alive(*pid))
                || row
                    .stamp_secs
                    .saturating_add(IN_FLIGHT_STALE_AFTER.as_secs())
                    > now
        });
        deletes.deletes.len() != before
    }

    /// Load the in-flight record under the same external-boundary rule the
    /// ownership ledger loads under: clauth-minted rows are always
    /// shape-valid, so anything else is a hand edit or corruption and fails
    /// the WHOLE record closed — an unreadable record must never read as "no
    /// delete in flight".
    pub(crate) fn load_in_flight() -> Result<InFlightDeletes> {
        let path = in_flight_path()?;
        let deletes = match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str::<InFlightDeletes>(&body).map_err(|e| {
                crate::format::parse_error(path.display(), crate::format::json_parse_position(&e))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(InFlightDeletes::default());
            }
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        for row in &deletes.deletes {
            if !crate::claude::is_namespaced_keychain_service(&row.service) {
                anyhow::bail!(
                    "the in-flight-delete record names a service the naming rule could not \
                     produce (`{}`). Refusing to read it as absent",
                    row.service
                );
            }
        }
        Ok(deletes)
    }

    fn save_in_flight(deletes: &InFlightDeletes) -> Result<()> {
        let path = in_flight_path()?;
        if let Some(parent) = path.parent() {
            crate::profile::mkdir_700(parent)
                .context("failed to create the in-flight-delete record directory")?;
            crate::profile::enforce_clauth_perms(parent);
        }
        let bytes = serde_json::to_vec_pretty(deletes)
            .context("failed to serialize the in-flight-delete record")?;
        atomic_write_600(&path, bytes).context("failed to persist the in-flight-delete record")
    }

    /// Stamp (or re-stamp) the in-flight record for one service, sweeping
    /// crash-stale rows with it. The caller holds the state flock — the stamp
    /// and the liveness re-check share one hold, so a seed serialized after it
    /// refuses and one serialized before it was spared by the re-check's own
    /// inputs. Mints the [`InFlightDelete`] witness only after the row
    /// persisted. The row carries no pid yet: the child does not exist until
    /// the delete spawns, and each subprocess leg stamps its child's pid at
    /// spawn ([`record_in_flight_pid`]).
    pub(crate) fn record_in_flight_locked(service: &str) -> Result<InFlightDelete> {
        let mut deletes = load_in_flight()?;
        sweep_stale(&mut deletes);
        match deletes
            .deletes
            .iter_mut()
            .find(|row| row.service == service)
        {
            Some(row) => row.stamp_secs = now_secs(),
            None => deletes.deletes.push(InFlightDeleteRow {
                service: service.to_string(),
                stamp_secs: now_secs(),
                pids: Vec::new(),
            }),
        }
        save_in_flight(&deletes)?;
        Ok(InFlightDelete {
            service: service.to_string(),
            pids: std::cell::RefCell::new(Vec::new()),
        })
    }

    /// Stamp the delete child's pid into the row the moment the child exists
    /// — right after the spawn, before the leg waits on it. The seed's
    /// consult refuses a row with a live child however old the row is. The
    /// not-found arm FAILS CLOSED: the row can be gone when the hook's own
    /// flock wait delayed it past the age bound and a consult swept it, and
    /// a child that cannot be stamped must not run its delete unguarded —
    /// the witnessing runner kills the child on this error, so the delete
    /// never fires against a seed the swept row already admitted.
    pub(crate) fn record_in_flight_pid(service: &str, pid: u32) -> Result<()> {
        with_state_lock(|_held| {
            let mut deletes = load_in_flight()?;
            let Some(row) = deletes
                .deletes
                .iter_mut()
                .find(|row| row.service == service)
            else {
                anyhow::bail!(
                    "cannot stamp the spawned child (pid {pid}) into the in-flight record for \
                     {service}: no row exists. A delete child must never run unguarded"
                );
            };
            // PID-identity premise: the pid-scoped clear keys on pid equality,
            // so a pid recycled from one collector's dead child to the other's
            // live one inside the overlap window would clear live tracking.
            // Accepted: pid allocation is monotonic over that seconds-long
            // window; no pid-based guard can close it.
            if !row.pids.contains(&pid) {
                row.pids.push(pid);
            }
            save_in_flight(&deletes)?;
            Ok(())
        })
    }

    /// The write-side consult: refuse the write while the guarded delete for
    /// the same service can still fire — its outcome (landed or failed) is
    /// not yet recorded, so the write could be destroyed by it. A row with a
    /// live child refuses however old it is; rows with no live child are
    /// swept and persisted by the age bound here, so the refusal is bounded
    /// — except a recycled pid now held by a long-lived process, which keeps
    /// refusing for that process's lifetime (the safe direction: the row
    /// dies when the process does or a clear reaches it). Runs inside the
    /// same flock hold as the ownership row's persist, so it serializes
    /// against every stamp.
    pub(crate) fn refuse_while_in_flight_locked(service: &str) -> Result<()> {
        let mut deletes = load_in_flight()?;
        if sweep_stale(&mut deletes) {
            save_in_flight(&deletes)?;
        }
        if let Some(row) = deletes.deletes.iter().find(|row| row.service == service) {
            return Err(DeleteInFlight {
                service: service.to_string(),
                stamp_secs: row.stamp_secs,
                pids: row.pids.clone(),
            }
            .into());
        }
        Ok(())
    }

    /// Clear exactly the witness's own children from the row, then the row
    /// itself once no pid remains: a concurrent collector's clear removes
    /// only ITS legs' pids, so one collector's finished delete can never
    /// erase another's live tracking. Takes its own flock hold, off the
    /// subprocess path.
    pub(crate) fn clear_in_flight(service: &str, pids: &[u32]) -> Result<()> {
        with_state_lock(|_held| {
            let mut deletes = load_in_flight()?;
            let before = deletes.deletes.len();
            let mut changed = false;
            deletes.deletes.retain_mut(|row| {
                if row.service != service {
                    return true;
                }
                let had = row.pids.len();
                row.pids.retain(|pid| !pids.contains(pid));
                changed |= row.pids.len() != had;
                !row.pids.is_empty()
            });
            if changed || deletes.deletes.len() != before {
                save_in_flight(&deletes)?;
            }
            Ok(())
        })
    }

    /// Proof that a namespaced Keychain delete was durably recorded as
    /// in-flight: minted only by [`record_in_flight_locked`] AFTER the row
    /// persisted under the state flock, and the fields are private to this
    /// module (the [`crate::lock::StateLockHeld`] pattern). The macOS delete
    /// sink takes this witness as its only proof, so no `/usr/bin/security`
    /// delete for a per-session item can run without a durable in-flight
    /// record behind it — the record-first order is a type, not a call-site
    /// convention, exactly like [`OwnedKeychainWrite`]. The witness also
    /// records the pids its OWN legs spawned, which is what scopes its
    /// clear: a concurrent collector's delete clears only its own tracking.
    #[derive(Debug)]
    pub(crate) struct InFlightDelete {
        service: String,
        pids: std::cell::RefCell<Vec<u32>>,
    }

    impl InFlightDelete {
        pub(crate) fn service(&self) -> &str {
            &self.service
        }

        /// Record a spawned child's pid on the witness and durably in the
        /// row: the durable half keeps the consult refusing while the child
        /// lives, and the witness's own list is what [`InFlightDelete::clear`]
        /// later removes.
        pub(crate) fn record_child(&self, pid: u32) -> Result<()> {
            record_in_flight_pid(&self.service, pid)?;
            self.pids.borrow_mut().push(pid);
            Ok(())
        }

        /// Clear the record once the delete's outcome is known (landed or
        /// failed), removing exactly this witness's own children; a crashed
        /// delete's record is swept once every stamped child is dead and the
        /// age bound passes, instead.
        pub(crate) fn clear(self) -> Result<()> {
            clear_in_flight(&self.service, &self.pids.borrow())
        }
    }

    /// A namespaced Keychain write was refused because a sweep's delete for
    /// the same service is still in flight. Transient by construction — the
    /// delete clears the record when it finishes, and a crashed delete's
    /// record is swept once every stamped child is dead and
    /// [`IN_FLIGHT_STALE_AFTER`] passes — so the seed maps this to its
    /// watchdog-tick retry. The one unbounded case is a recycled pid now
    /// held by a long-lived process, whose alive reading keeps refusing for
    /// that process's lifetime — the safe direction, never a silent skip.
    #[derive(Debug)]
    pub(crate) struct DeleteInFlight {
        pub(crate) service: String,
        pub(crate) stamp_secs: u64,
        pub(crate) pids: Vec<u32>,
    }

    impl std::fmt::Display for DeleteInFlight {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if let Some(pid) = self.pids.iter().find(|pid| pid_alive(**pid)) {
                write!(
                    f,
                    "a sweep's delete for the same per-session Keychain item is still in flight \
                     (`{}`, whose `security` child, pid {pid}, is still alive). The write would race \
                     it. It lands once the delete records its outcome or its record is swept as \
                     stale",
                    self.service,
                )
            } else {
                write!(
                    f,
                    "a sweep's delete for the same per-session Keychain item is still in flight \
                     (`{}`, stamped {} s ago). The write would race it. It lands once the delete \
                     records its outcome or its record is swept as stale",
                    self.service,
                    now_secs().saturating_sub(self.stamp_secs),
                )
            }
        }
    }

    impl std::error::Error for DeleteInFlight {}

    pub(crate) fn authorize_write(
        runtime: &Path,
        profile: &ProfileName,
        session: &SessionId,
    ) -> Result<OwnedKeychainWrite> {
        authorize_write_with(runtime, profile, session, save)
    }

    pub(crate) fn authorize_write_with(
        runtime: &Path,
        profile: &ProfileName,
        session: &SessionId,
        persist: impl FnOnce(&Owners) -> Result<()>,
    ) -> Result<OwnedKeychainWrite> {
        let service = crate::claude::namespaced_keychain_service(
            &crate::canonical::identity(runtime).with_context(|| {
                format!("failed to canonicalize config dir: {}", runtime.display())
            })?,
        );
        record_with(&service, profile, session, persist)?;
        Ok(OwnedKeychainWrite { service })
    }

    /// Mint the write witness for a per-session item ANOTHER process owns:
    /// only when the ledger already holds a row for the item `runtime`
    /// derives, and no sweep's delete of it is in flight, checked in one
    /// state-lock hold. The row is left exactly as it is: the writer works
    /// beside the owning session, it never takes the item over, and an item
    /// nobody owns is the orphan GC's, never this.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "the only caller is the macOS copied-bearer re-point"
        )
    )]
    pub(crate) fn authorize_owned_write(runtime: &Path) -> Result<OwnedKeychainWrite> {
        let service = crate::claude::namespaced_keychain_service(
            &crate::canonical::identity(runtime).with_context(|| {
                format!("failed to canonicalize config dir: {}", runtime.display())
            })?,
        );
        with_state_lock(|_held| {
            refuse_while_in_flight_locked(&service)?;
            anyhow::ensure!(
                load()?.owners.iter().any(|owner| owner.service == service),
                "no ownership row names the per-session Keychain item `{service}`"
            );
            Ok(())
        })?;
        Ok(OwnedKeychainWrite { service })
    }

    /// [`authorize_owned_write`] for an item no row names yet: Claude Code
    /// created it itself in a live session's own runtime tree. ADOPTS it by
    /// recording the `(service, profile, session)` row the session's seed
    /// would have, so the orphan GC collects it like any seeded item once the
    /// tree goes, and the session's own swap legs ([`authorize_write`]) take
    /// the row over unchanged. A row that exists by now is left as it is.
    /// Refused unless [`super::session_item_may_adopt`] holds, and while a
    /// delete is in flight.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "the only caller is the macOS copied-bearer re-point"
        )
    )]
    pub(crate) fn authorize_live_write(
        runtime: &Path,
        profile: &ProfileName,
        session: &str,
        background: bool,
    ) -> Result<OwnedKeychainWrite> {
        anyhow::ensure!(
            super::session_item_may_adopt(runtime, session, background),
            "{} is not live session {session}'s own runtime tree",
            runtime.display()
        );
        let service = crate::claude::namespaced_keychain_service(
            &crate::canonical::identity(runtime).with_context(|| {
                format!("failed to canonicalize config dir: {}", runtime.display())
            })?,
        );
        with_state_lock(|_held| {
            refuse_while_in_flight_locked(&service)?;
            let mut owners = load()?;
            if !owners.owners.iter().any(|owner| owner.service == service) {
                owners.owners.push(Owner {
                    service: service.clone(),
                    profile: profile.clone(),
                    session: session.to_string(),
                });
                save(&owners)?;
            }
            Ok(())
        })?;
        Ok(OwnedKeychainWrite { service })
    }

    /// Proof that a namespaced Keychain write is durably owned: minted only by
    /// [`authorize_write`] AFTER the `(service, profile, session)` row persisted
    /// under the state flock, and the field is private to this module so nothing
    /// else can conjure one (the `StateLockHeld` pattern). The macOS Keychain
    /// sinks take this witness as their only proof, so no `/usr/bin/security`
    /// write for a per-session item can run without a durable ownership row
    /// behind it — the ownership-first order is a type, not a call-site
    /// convention.
    #[derive(Debug)]
    pub(crate) struct OwnedKeychainWrite {
        service: String,
    }

    impl OwnedKeychainWrite {
        pub(crate) fn service(&self) -> &str {
            &self.service
        }
    }
}

/// Collect `spawn-settings[-isolated]-<sid>.json` files no live session and no
/// job holds: a crash leftover, or a file a teardown kept for a job that is
/// gone or while the job store could not be read. The name carries the
/// flavor, so liveness is the registry's own [`session_row_is_live`] over
/// every marker layout (per-session and fake mode's bare `sessions/`): a held
/// marker spares the file, an undecidable one reads as live, and a file with
/// no marker at all is a crash leftover. The job term is
/// [`crate::claude_jobs::JobTrees::spares_settings`] over both layouts' trees;
/// nothing else is spared (the file can hold a folded user `env` token).
fn gc_spawn_settings_files() {
    gc_spawn_settings_files_synced(|| {});
}

/// [`gc_spawn_settings_files`] with the in-hold interleave point exposed for
/// the pin: `in_hold` runs as the first statement inside the state-flock
/// closure, before any liveness check or job-store read, so a job written
/// there still spares the file it names. A no-op in production.
fn gc_spawn_settings_files_synced(in_hold: impl FnOnce()) {
    let Ok(root) = profiles_root_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut candidates: Vec<(std::path::PathBuf, ProfileName, bool, String)> = Vec::new();
    for profile in entries.flatten() {
        let profile_path = profile.path();
        let Some(profile_name) = profile.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(children) = std::fs::read_dir(&profile_path) else {
            continue;
        };
        for child in children.flatten() {
            let Some(name) = child.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some((isolated, sid)) = parse_spawn_settings_name(&name) else {
                continue;
            };
            candidates.push((
                child.path(),
                ProfileName::from(profile_name.clone()),
                isolated,
                sid,
            ));
        }
    }
    // Peek before locking, like `gc_bare_markers`: nothing to collect must not
    // pay the state flock's wait.
    if candidates.is_empty() {
        return;
    }
    let _ = with_state_lock(|_held| {
        in_hold();
        // Read the store only once a file is actually dead, so a sweep of all
        // live sessions holds the flock for no job-store walk.
        let mut jobs_read: Option<crate::claude_jobs::JobTrees> = None;
        for (file, profile, isolated, sid) in candidates {
            if session_row_is_live(&profile, isolated, &sid) {
                continue;
            }
            let jobs = jobs_read.get_or_insert_with(crate::claude_jobs::job_trees);
            let Ok(trees) = session_runtime_trees(&profile, isolated, &sid) else {
                continue;
            };
            // A launcher's file belongs to the pair its session occupied. The
            // fixed background tree is offered alongside the per-session pair of
            // every SHARED file: `spares_settings`' fallback is loose by design,
            // and a background job whose flags cannot be read must spare the
            // `--settings` file its own respawn names. (An isolated file never
            // belongs to that pair — `--bg` is refused there.)
            let mut own_trees = trees.to_vec();
            if !isolated && let Ok(tree) = background_runtime_tree(&profile) {
                own_trees.push(tree);
            }
            if jobs.spares_settings(&file, &own_trees) {
                continue;
            }
            let _ = std::fs::remove_file(&file);
        }
        Ok::<_, anyhow::Error>(())
    });
}

/// `spawn-settings-<sid>.json` (shared) or `spawn-settings-isolated-<sid>.json`
/// back to `(isolated, sid)`; `None` when the name does not parse, so a stray
/// file clauth did not write is left alone.
fn parse_spawn_settings_name(name: &str) -> Option<(bool, String)> {
    let rest = name.strip_prefix("spawn-settings-")?;
    let (isolated, rest) = match rest.strip_prefix("isolated-") {
        Some(sid) => (true, sid),
        None => (false, rest),
    };
    let sid = rest.strip_suffix(".json")?;
    is_session_id(sid).then(|| (isolated, sid.to_string()))
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
    // Services tab's probe child, which dies at 3s — while the state flock waits up
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
/// the session could still hold is flock-held — the attributed member first (a
/// swapped session runs there), then the launch member — and, for a chain row,
/// no Claude Code job names its per-session tree: that tree is the hand-off
/// record a later daemon adopts. This keeps GC aligned with
/// [`crate::live_sessions::LiveTally::collect`] — a row can never be alive in
/// the tally and dead to GC, which is what would silently reap a live swapped row
/// after a force-delete of its launch profile. Folded in here rather than given
/// its own entry point so every existing `gc_stale_runtimes` caller gets it.
fn gc_live_session_rows() {
    let jobs = crate::claude_jobs::job_trees();
    // Test seam: a job record landing after the pass-start snapshot but before
    // the loop reaches its row must be seen by the re-read below, not reaped
    // off the stale snapshot. Nothing outside tests sets this.
    #[cfg(test)]
    run_gc_rows_reread_hook();
    for row in crate::live_sessions::list() {
        if row.marker_is_held() {
            continue;
        }
        // A handed-off chain row is the record a daemon later adopts: spare it
        // while a job names its per-session tree, and fail closed under an
        // unreadable store (spares reads true) like every other job reader. A
        // row outside the chain is reaped whatever a job keeps.
        let chain_tree = (row.follows_chain && !row.isolated)
            .then(|| {
                chain_session_tree(
                    &ProfileName::from(row.start_profile.as_str()),
                    &row.session_id,
                )
            })
            .and_then(|tree| tree.ok());
        if chain_tree.as_ref().is_some_and(|tree| jobs.spares(tree)) {
            continue;
        }
        // A launch whose job record landed after the pass-start snapshot and
        // whose launcher handed off before this loop reached its row is reaped
        // off a store the snapshot outlived. Re-read before reaping a dead
        // chain row — rare (a job-less chain row) and cheap, so the hand-off
        // record is never lost to a stale snapshot.
        if chain_tree
            .as_ref()
            .is_some_and(|tree| crate::claude_jobs::job_trees().spares(tree))
        {
            continue;
        }
        if let Err(e) = crate::live_sessions::unregister(&row.session_id) {
            logline!("clauth: dropping stale live-session row failed: {e}");
        }
    }
}

/// Test seam for [`gc_live_session_rows`]'s per-row re-read: a hook run once
/// between the pass-start job snapshot and the row loop, posing a job record
/// that landed after the snapshot. Test-only.
#[cfg(test)]
pub(crate) type GcRowsRereadHook = Box<dyn FnMut()>;

#[cfg(test)]
thread_local! {
    static GC_ROWS_REREAD_HOOK: std::cell::RefCell<Option<GcRowsRereadHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_gc_rows_reread_hook(hook: Option<GcRowsRereadHook>) {
    crate::testutil::set_hook(&GC_ROWS_REREAD_HOOK, hook);
}

#[cfg(test)]
fn run_gc_rows_reread_hook() {
    if let Some(mut hook) = crate::testutil::take_hook(&GC_ROWS_REREAD_HOOK) {
        hook();
    }
}

/// Test seam for [`ProfileRuntime::acquire_adopted`]'s member re-check: a hook
/// run once between the pre-lock member read and the in-flock re-read, posing
/// a swap that moves the session while the adoption waits. Test-only.
#[cfg(test)]
pub(crate) type AdoptRereadHook = Box<dyn FnMut()>;

#[cfg(test)]
thread_local! {
    static ADOPT_REREAD_HOOK: std::cell::RefCell<Option<AdoptRereadHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_adopt_reread_hook(hook: Option<AdoptRereadHook>) {
    crate::testutil::set_hook(&ADOPT_REREAD_HOOK, hook);
}

#[cfg(test)]
fn run_adopt_reread_hook() {
    if let Some(mut hook) = crate::testutil::take_hook(&ADOPT_REREAD_HOOK) {
        hook();
    }
}

/// Rescue a runtime root into the global store, one way: the transcripts under
/// `projects/`, then the session sidecars under `sidecars`' rule. Returns
/// `(transcripts, sidecars)`. Best-effort throughout: an error is logged and
/// counted as left, never fails the caller. Shared by
/// [`crate::start::rescue_teardown`] and every tombstone ([`rescue_tombstone`]),
/// so an unrescued tree is lifted at its deletion site rather than only on a
/// clean exit. Its rescue-lock waits share one budget
/// ([`crate::sessions::RescueLockBudget`]), or the enclosing sweep's.
pub(crate) fn rescue_runtime(
    root: &Path,
    claude_home: &Path,
    sidecars: crate::sessions::SidecarSource,
) -> (crate::sessions::Rescued, crate::sessions::Rescued) {
    // An isolated tree names the account its run served, and only until its
    // transcripts move into the global store, so its spend-cap 429 is settled
    // before the lift, and before the rescue lock's budget starts to run. A
    // shared tree's run is placed off its live process.
    if sidecars == crate::sessions::SidecarSource::Isolated {
        crate::usage::note_isolated_credit_refusal(root);
    }
    let _rescue_wait = crate::sessions::RescueLockBudget::arm();
    let transcripts =
        crate::sessions::rescue_transcripts(&root.join("projects"), &claude_home.join("projects"));
    let sidecars = crate::sessions::rescue_sidecars(root, claude_home, sidecars);
    if transcripts.moved > 0 || sidecars.moved > 0 {
        logline!(
            "clauth: rescued {} session transcript(s) + {} sidecar file(s) from {} \
             into the global store",
            transcripts.moved,
            sidecars.moved,
            root.display()
        );
    }
    (transcripts, sidecars)
}

/// Retire a dead runtime tree from its live name, inside the removal's state
/// flock hold. A tree whose root is itself a link (an operator's symlink, a
/// Windows junction) holds nothing of its own: the link alone is removed, and
/// nothing behind it is renamed or walked. Any other tree may hold the only
/// copy of what its session wrote (an isolated tree's whole store; a shared
/// tree's entries `~/.claude` lacked at its build, or a copy host's tail past
/// its last mirror tick), so it is renamed to its rescue tombstone, O(1), and
/// returned for [`rescue_tombstone`] once the hold drops. A failed rename
/// keeps the tree where it is for a later sweep.
fn retire_tree(runtime: &Path) -> Option<PathBuf> {
    if crate::sessions::is_symlink(runtime) {
        unlink_link(runtime, "linked runtime tree");
        return None;
    }
    let tombstone = tombstone_path(runtime);
    #[cfg(test)]
    let renamed = if RETIRE_RENAME_REFUSED.with(std::cell::Cell::get) {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    } else {
        std::fs::rename(runtime, &tombstone)
    };
    #[cfg(not(test))]
    let renamed = std::fs::rename(runtime, &tombstone);
    match renamed {
        Ok(()) => Some(tombstone),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            logline!(
                "clauth: kept runtime tree {}: renaming it for rescue failed: {e}",
                runtime.display()
            );
            None
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Set by [`RetireRenameRefused`]: [`retire_tree`]'s rename fails on this
    /// thread, as one another process's open handle refuses on Windows.
    static RETIRE_RENAME_REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Refuses every [`retire_tree`] rename on the calling thread until dropped.
#[cfg(test)]
pub(crate) struct RetireRenameRefused(bool);

#[cfg(test)]
impl RetireRenameRefused {
    pub(crate) fn set() -> Self {
        Self(RETIRE_RENAME_REFUSED.with(|c| c.replace(true)))
    }
}

#[cfg(test)]
impl Drop for RetireRenameRefused {
    fn drop(&mut self) {
        RETIRE_RENAME_REFUSED.with(|c| c.set(self.0));
    }
}

/// The rescue tombstone a dead `runtime` is renamed to.
fn tombstone_path(runtime: &Path) -> PathBuf {
    with_name_suffix(runtime, RESCUE_TOMBSTONE_SUFFIX)
}

/// `path` with `suffix` appended to its file name.
fn with_name_suffix(path: &Path, suffix: &str) -> PathBuf {
    path.file_name()
        .map(|name| {
            let mut suffixed = name.to_os_string();
            suffixed.push(suffix);
            path.with_file_name(suffixed)
        })
        .unwrap_or_else(|| path.to_path_buf())
}

/// The suffix of the claim file beside a rescue tombstone ([`RescueClaim`]).
const RESCUE_CLAIM_SUFFIX: &str = ".claim";

/// One sweep's exclusive claim on a rescue tombstone, held across its whole
/// rescue, so two sweeps (a renaming sweep's tail and another process's
/// stranded-tombstone arm) never walk, count and remove one tree at once: a
/// second sweep that removed a tombstone the first is still finishing would
/// free its name, and a dead tree retired onto that name would go unwalked
/// with the first sweep's removal. Two
/// tombstones landing on one global file are ordered per file by the rescue's
/// own lock (`sessions::RescueFileLock`), never by this claim. The lock is a
/// flock (`LockFileEx` on Windows) on a file BESIDE the tombstone, never
/// inside it: an open handle inside would block the tombstone's removal on
/// Windows. It is a try-lock, so a held claim means another sweep is finishing
/// the tombstone and this one skips it; a claim file a crash left is held by
/// nobody and is taken as usual.
struct RescueClaim {
    path: PathBuf,
    file: File,
}

impl RescueClaim {
    /// `None` when another sweep holds the claim, or it cannot be taken
    /// (logged); either way the tombstone stays for a later sweep.
    fn take(tombstone: &Path) -> Option<Self> {
        let path = with_name_suffix(tombstone, RESCUE_CLAIM_SUFFIX);
        let file = match open_rescue_claim(&path) {
            Ok(file) => file,
            // The profile dir went, and the tombstone with it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                logline!(
                    "clauth: cannot claim {} for rescue: {e}",
                    tombstone.display()
                );
                return None;
            }
        };
        #[cfg(test)]
        fire_claim_hook(&CLAIM_OPENED_HOOK, &path);
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return None,
            Err(std::fs::TryLockError::Error(e)) => {
                logline!(
                    "clauth: cannot claim {} for rescue: {e}",
                    tombstone.display()
                );
                return None;
            }
        }
        claim_is_named(&file, &path).then_some(Self { path, file })
    }

    /// Drop the claim. Its file goes once the tombstone has, so a kept
    /// tombstone's next sweep locks the same file. Unix unlinks it while still
    /// holding the lock: a sweep that opened it before can then only lock an
    /// unlinked file, which [`claim_is_named`] refuses. Windows cannot delete a
    /// file another handle holds open ([`open_rescue_claim`]), so the handle
    /// closes first, and a failed delete means another sweep holds it now and
    /// removes it in turn.
    fn release(self, tombstone: &Path) {
        let Self { path, file } = self;
        let gone = tombstone
            .symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        #[cfg(not(unix))]
        drop(file);
        if gone {
            #[cfg(test)]
            fire_claim_hook(&CLAIM_UNLINK_HOOK, &path);
            let _ = std::fs::remove_file(&path);
        }
        #[cfg(unix)]
        drop(file);
    }
}

#[cfg(test)]
type ClaimHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    /// Fires once in [`RescueClaim::take`] between the claim file's open and
    /// its lock: the window another sweep's release can land in.
    static CLAIM_OPENED_HOOK: std::cell::RefCell<Option<ClaimHook>> =
        const { std::cell::RefCell::new(None) };
    /// Fires once in [`RescueClaim::release`] right before the claim file's
    /// unlink: the moment another sweep must still find it held.
    static CLAIM_UNLINK_HOOK: std::cell::RefCell<Option<ClaimHook>> =
        const { std::cell::RefCell::new(None) };
    /// Fires once in [`rescue_claimed_tombstone`] right before a rescued
    /// tombstone's removal: the moment another sweep may reach it.
    static TOMBSTONE_REMOVAL_HOOK: std::cell::RefCell<Option<ClaimHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn fire_claim_hook(
    slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<ClaimHook>>>,
    path: &Path,
) {
    if let Some(hook) = crate::testutil::take_hook(slot) {
        hook(path);
    }
}

#[cfg(test)]
fn set_claim_hook(
    slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<ClaimHook>>>,
    hook: ClaimHook,
) {
    crate::testutil::set_hook(slot, Some(hook));
}

/// Open or create a rescue claim file, owner-only. On Windows it shares read
/// and write but never delete, so its name cannot be removed while any sweep
/// holds it open, and a handle always locks the file the name points at.
fn open_rescue_claim(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE, without FILE_SHARE_DELETE.
        const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(SHARE_READ_WRITE)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        crate::profile::open_state_file(path)
    }
}

/// Whether the locked `file` is still the one `path` names. A sweep that
/// opened the claim just before its holder unlinked it locks an orphan once
/// the holder lets go, while a later sweep may create and lock a new file at
/// the same name: only one of them may rescue.
#[cfg(unix)]
fn claim_is_named(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(held), Ok(named)) => held.dev() == named.dev() && held.ino() == named.ino(),
        _ => false,
    }
}

/// Windows: [`open_rescue_claim`] keeps a held claim's name from being
/// removed, so a locked handle is always the named file.
#[cfg(not(unix))]
fn claim_is_named(_file: &File, _path: &Path) -> bool {
    true
}

/// Whether a dead shared tree is a copy of the global store: a copy host's
/// build copies every entry and links none, so a tree is a copy iff none of its
/// top-level entries is a link. A linking build links what `~/.claude` holds
/// plus the `projects` and `jobs` stores it can create there; one that linked
/// nothing (an empty `~/.claude` it could not write) reads as a copy too. An
/// unreadable tree or entry reads as no copy, whose sibling rule never drops a
/// file.
fn is_copy_tree(tree: &Path) -> bool {
    std::fs::read_dir(tree).is_ok_and(|mut entries| {
        entries.all(|entry| entry.is_ok_and(|e| e.file_type().is_ok_and(|t| !t.is_symlink())))
    })
}

/// Finish a rescue from a runtime tombstone: lift its transcripts and sidecars
/// into the global store, then remove it. Runs outside the state lock — the
/// name is rejected by [`is_paired_dir_name`], so nothing can adopt it — and is
/// the single tail [`gc_one_pair`], `acquire`, `Drop`, the stranded-tombstone
/// sweep and a profile's delete share, so a crash between the rename and this call
/// cannot strand the tree. One sweep at a time walks a tombstone
/// ([`RescueClaim`]); one another sweep is finishing is skipped. A tombstone
/// the rescue left anything in is kept for that sweep to retry: it may hold the
/// only copy. A tombstone that is itself a link is unlinked and never walked:
/// what it resolves to is no tree of clauth's.
fn rescue_tombstone(tombstone: &Path) {
    let Some(claim) = RescueClaim::take(tombstone) else {
        return;
    };
    rescue_claimed_tombstone(tombstone);
    claim.release(tombstone);
}

/// [`rescue_tombstone`] under its claim.
fn rescue_claimed_tombstone(tombstone: &Path) {
    // The sweep that held the claim before this one finished it.
    if tombstone
        .symlink_metadata()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return;
    }
    if crate::sessions::is_symlink(tombstone) {
        unlink_link(tombstone, "linked runtime tombstone");
        return;
    }
    let claude_home = match claude_dir() {
        Ok(claude_home) => claude_home,
        Err(e) => {
            logline!("clauth: cannot rescue runtime {}: {e}", tombstone.display());
            return;
        }
    };
    // Only a copy took a copy of every global sidecar at its build; on any
    // other shared tree an occupied name is the session's own.
    let isolated = tombstone
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(ISOLATED_RUNTIME_STEM));
    let sidecars = if isolated {
        crate::sessions::SidecarSource::Isolated
    } else if is_copy_tree(tombstone) {
        crate::sessions::SidecarSource::SharedCopy
    } else {
        crate::sessions::SidecarSource::Shared
    };
    let (transcripts, sidecars) = rescue_runtime(tombstone, &claude_home, sidecars);
    let left = transcripts.left + sidecars.left;
    if left > 0 {
        logline!(
            "clauth: kept {}: {left} file(s) could not be rescued; a later sweep retries",
            tombstone.display()
        );
        return;
    }
    #[cfg(test)]
    fire_claim_hook(&TOMBSTONE_REMOVAL_HOOK, tombstone);
    match std::fs::remove_dir_all(tombstone) {
        Ok(()) => {}
        // Removed under the claim by something other than a sweep: nothing of
        // it is left to lose.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => logline!(
            "clauth: failed to remove rescued runtime {}: {e}",
            tombstone.display()
        ),
    }
}

/// Collect one paired (`runtime<rest>`, `sessions<rest>`) tree when nothing holds
/// a marker in it and no live Claude Code background job names it. The two go
/// together; a `runtime` path that does not exist collects the orphaned marker
/// dir alone. Any tree can hold the only copy of what its session wrote, so
/// it is renamed to a tombstone under the lock and rescued outside it
/// ([`retire_tree`]), and the flock is never held across a tree-sized walk; a
/// runtime path that is itself a link is unlinked alone.
fn gc_one_pair(runtime: &Path, sessions: &Path, job_trees: &JobTrees) -> Result<()> {
    // The job check sits here as a FAST PATH, before the state flock
    // `gc_one_pair_synced` takes: the store was read once above and the check
    // is a canonicalize + set lookup, not a walk. It is not the decision — a
    // job written after the snapshot but before the marker frees is caught by
    // the fresh re-read `gc_one_pair_synced` makes under the lock. Spared
    // before the flock means a named tree is never renamed to its rescue
    // tombstone — the one order that would rescue-and-delete a tree a live job
    // still reads.
    if job_trees.spares(runtime) {
        // Log only a NAMED hit: under an unreadable store (global or scoped)
        // `spares` is true for pairs no job was shown to name, and the
        // sweep-level line already covered the unreadable case once per sweep.
        // A per-pair line there would claim a live job names every tree.
        if job_trees.names(runtime) {
            logline!(
                "clauth: spared runtime tree {}: a live Claude Code background job names it",
                runtime.display()
            );
        }
        return Ok(());
    }
    gc_one_pair_synced(runtime, sessions, || {})
}

#[cfg(test)]
type GcRecheckHook = Box<dyn FnMut(&Path, &Path)>;

#[cfg(test)]
thread_local! {
    static GC_RECHECK_HOOK: std::cell::RefCell<Option<GcRecheckHook>> =
        const { std::cell::RefCell::new(None) };
    static GC_HELD_MARKER: std::cell::RefCell<Option<File>> = const { std::cell::RefCell::new(None) };
}

/// The GC pin seam: fires between the decide/stop and the re-check hold, so a
/// test interposes a job/marker/lock file the re-check must re-read. Test-only.
#[cfg(test)]
fn on_gc_recheck_hold(runtime: &Path, sessions: &Path) {
    GC_RECHECK_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(runtime, sessions);
        }
    });
}

#[cfg(test)]
fn set_gc_recheck_hook(hook: Option<GcRecheckHook>) {
    crate::testutil::set_hook(&GC_RECHECK_HOOK, hook);
}

#[cfg(test)]
fn gc_held_marker_take() -> Option<File> {
    GC_HELD_MARKER.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
fn gc_held_marker_set(file: File) {
    GC_HELD_MARKER.with(|slot| *slot.borrow_mut() = Some(file));
}

/// [`gc_one_pair`] with the re-mint interleave point exposed for the pin:
/// `remint_interleave` runs after the collection's state-flock closure and
/// before the Keychain item's delete — the window in which a concurrently
/// starting session re-mints this same path (its whole lock section: marker
/// claim, tree rebuild, then the post-lock item seed) while the sweep sits
/// between its collection and its delete. A no-op in production.
///
/// The removal hold renames the tree to its tombstone ([`retire_tree`]), and
/// the rescue runs once the lock drops, because nothing but that tree may hold
/// what its dead session wrote.
fn gc_one_pair_synced(
    runtime: &Path,
    sessions: &Path,
    remint_interleave: impl FnOnce(),
) -> Result<()> {
    // macOS: the tree's namespaced Keychain service, derived while the dir
    // still exists — the derivation canonicalizes it, so it cannot run once
    // the tree is removed. Collected after the closures below, where a
    // `security` subprocess is legal; `orphaned_keychain_item` documents why
    // its inputs are re-sampled — and the delete's in-flight record stamped,
    // in the same hold (`gc_keychain_recheck`) — under a second closure
    // immediately before the delete.
    #[cfg(target_os = "macos")]
    let item_service = tree_keychain_service(runtime);

    // Decide under the lock whether the tree is collectible and what its
    // supervisor needs: no removal, no subprocess, no signal here. The
    // liveness reads are the same ones the re-check repeats below: an unknown
    // reads as live (this leg runs from the daemon's timer, in a different
    // process, against every profile, and under `LinkMode::Fake` the tree it
    // would remove is the one a live sibling runs out of), and the job store
    // is re-read because a `clauth start <p> -- --bg` can write its job, exit,
    // and release its marker after `gc_one_pair`'s snapshot.
    #[derive(Clone, Copy)]
    enum Collect {
        /// No supervisor: collect on the re-check below.
        Collect,
        /// Stop the verified supervisor first (in-flight record stamped).
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Stop,
    }
    let collect = with_state_lock(|_held| {
        if prune_stale_sessions(sessions).unwrap_or(1) != 0 {
            return Ok(None);
        }
        if crate::claude_jobs::job_trees().spares(runtime) {
            return Ok(None);
        }
        match supervisor_state(runtime) {
            SupervisorState::None | SupervisorState::Gone => Ok(Some(Collect::Collect)),
            SupervisorState::Unverifiable { stale_start } => {
                announce_supervisor_spare(runtime, stale_start.as_deref());
                Ok(None)
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            SupervisorState::Ours { .. } => {
                record_stop_in_flight_locked(runtime)?;
                Ok(Some(Collect::Stop))
            }
        }
    })?;
    let Some(collect) = collect else {
        return Ok(());
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = collect;

    // The stop runs with the lock dropped — no signal, subprocess or wait ever
    // spans the state flock (the macOS `security` rule). A stop that cannot
    // confirm the supervisor is gone spares the tree this pass;
    // `stop_tree_supervisor` logged the outcome.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if matches!(collect, Collect::Stop) && stop_tree_supervisor(runtime) == SupervisorStop::Spare {
        clear_stop_in_flight(runtime);
        return Ok(());
    }
    #[cfg(test)]
    on_gc_recheck_hold(runtime, sessions);

    // Re-check under a FRESH lock hold, then collect: between the decide above
    // and this hold a concurrently starting session can re-mint this same path,
    // so the removal keys on the world it runs in — markers, the job store AND
    // the lock file, all re-read fresh — the same shape the macOS Keychain
    // delete below takes.
    let renamed = with_state_lock(|_held| {
        let spares = prune_stale_sessions(sessions).unwrap_or(1) != 0
            || crate::claude_jobs::job_trees().spares(runtime)
            || !matches!(
                supervisor_state(runtime),
                SupervisorState::None | SupervisorState::Gone
            );
        if spares {
            return Ok(None);
        }
        // The rename is what stops an `acquire` adopting the tree while the
        // rescue reads it: the tombstone name matches no pairing predicate.
        let tombstone = retire_tree(runtime);
        let _ = std::fs::remove_dir(sessions);
        Ok(tombstone)
    })?;

    clear_stop_in_flight(runtime);
    remint_interleave();

    // macOS: collect the tree's Keychain item, before the rescue — the
    // collection is a salvage read then a delete (two subprocesses), the rescue
    // is a tree-sized copy, and a crash during the copy must not strand an item
    // whose dir is already gone. The pair is re-checked and the delete's
    // in-flight record stamped in ONE fresh state-lock hold taken immediately
    // before the delete: between the collection above and this point a
    // concurrently starting session can re-mint this same path, and the delete
    // must key on the world it runs in. Lock taken, inputs sampled, record
    // stamped, lock dropped, THEN the collection — the subprocesses still
    // never span the flock, and a seed serialized after the hold is refused
    // by the record it finds.
    #[cfg(target_os = "macos")]
    if let Some(in_flight) = gc_keychain_recheck(item_service.as_deref(), sessions, runtime)? {
        collect_orphaned_keychain_item(in_flight);
    }

    if let Some(tombstone) = renamed {
        rescue_tombstone(&tombstone);
    }
    Ok(())
}

/// The namespaced Keychain item a collected runtime tree leaves behind, if
/// any: the tree's derived service when nothing live explains the item — no
/// flock-held marker left in the paired sessions dir, and the dir that
/// explains the item GONE — `None` otherwise. PURE (the `session_seed_arm`
/// split) so the keep/delete rule is pinned on every platform; the `security`
/// delete it feeds is macOS-only and unreachable under `cfg(test)`, where
/// `keychain::enabled()` is false.
///
/// The rows, and why each keeps or collects:
/// - a LIVE pair — the lock's liveness re-check spared it — keeps its dir and
///   holds its marker, so its item stays: a live session's Claude Code reads
///   that item (CC resolves the Keychain before any file, namespaced per
///   `CLAUDE_CONFIG_DIR`).
/// - a pair the sweep could not collect (a failed tombstone rename, an
///   unreadable tree) keeps its dir, and its item with it.
/// - a collected pair (the tree renamed to its rescue tombstone, or a linked
///   runtime root unlinked) has no dir and no live marker: its item is
///   orphaned — only that dir's hash resolved it — and is collected.
/// - a runtime dir that never existed (the orphaned-marker arm; a crash
///   between minting the marker dir and building the tree) derived no service,
///   and no tree ever hosted a session seed to write an item.
/// - a RE-MINTED pair — a concurrently starting session re-claimed the path
///   between the collection and the delete (#82) — holds its marker again, so
///   its item stays even before its dir is rebuilt: a live marker outranks
///   dir existence. An unreadable sessions dir (`None`) reads as live for the
///   same reason every destructive level here folds an unknown into sparing.
///
/// Both inputs are sampled in a fresh state-lock hold taken immediately
/// before the delete, never inside the collection's: the delete is a
/// subprocess and must never span the flock, so the lock is taken, the inputs
/// read, and dropped, and only then does the delete run — serialized against
/// an acquire's own lock section, which claims the marker and rebuilds the
/// tree as one step. What closes the tail that drop leaves — a queued acquire
/// completing its lock section and seeding the item between this re-check
/// passing and the delete landing — is the in-flight-delete record stamped in
/// the SAME hold that samples these inputs ([`gc_keychain_recheck`]): a seed
/// serialized after the hold is refused by the record, one serialized before
/// it was spared by the inputs themselves (its live marker or rebuilt dir).
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumer is the macOS stale-runtime GC's Keychain collection; the decision is pinned on every platform"
    )
)]
fn orphaned_keychain_item(
    service: Option<&str>,
    live_markers: Option<usize>,
    dir_exists: bool,
) -> Option<&str> {
    service.filter(|_| live_markers == Some(0) && !dir_exists)
}

/// The macOS GC's item-collection re-check, flock-wrapped: the keep/delete
/// decision and the in-flight record's stamp share ONE state-lock hold. A
/// seed serialized after this hold is refused by the record; one serialized
/// before it was spared by the decision's own inputs (a live marker or a
/// rebuilt dir) — the pair closes #82's residual window, where a queued
/// acquire could seed the item after the re-check passed and before the
/// delete landed. Returns the witness the delete sink requires, or `None`
/// when spared.
///
/// A stamp that cannot be persisted skips the delete rather than running it
/// unguarded: the item is inert without its dir, a later census collects it,
/// and the skip is loud on the event line.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS stale-runtime GC's Keychain collection; the flock-held shape is pinned on every platform"
    )
)]
fn gc_keychain_recheck(
    item_service: Option<&str>,
    sessions: &Path,
    runtime: &Path,
) -> Result<Option<namespaced_keychain_ledger::InFlightDelete>> {
    with_state_lock(|_held| {
        let Some(service) = orphaned_keychain_item(
            item_service,
            prune_stale_sessions(sessions),
            runtime.symlink_metadata().is_ok(),
        ) else {
            return Ok(None);
        };
        match namespaced_keychain_ledger::record_in_flight_locked(service) {
            Ok(in_flight) => Ok(Some(in_flight)),
            Err(e) => {
                logline!(
                    "clauth: cannot stamp the in-flight-delete record for {service} ({e:#}). Its \
                     item is not collected. A delete whose in-flight record cannot be persisted \
                     must not run"
                );
                Ok(None)
            }
        }
    })
}

/// macOS: the namespaced Keychain service for a runtime dir the GC is walking,
/// derived while the dir still exists — the derivation canonicalizes it, so it
/// cannot run once the tree is removed. `None` when the dir is already gone
/// (the orphaned-marker arm: no tree was ever built, so no session seed wrote
/// an item), when it cannot be read, and under `cfg(test)`, where
/// `keychain::enabled()` is false and no `security` call may run. The probe
/// never reaches this fn: `gc_stale_runtimes` skips the whole tree sweep under
/// `MCP_PROBE_ENV`, which is what keeps tree and item paired there.
#[cfg(target_os = "macos")]
fn tree_keychain_service(runtime: &Path) -> Option<String> {
    if !crate::keychain::enabled() {
        return None;
    }
    crate::keychain::keychain_service_for_config_dir(runtime).ok()
}

/// macOS: collect one orphaned namespaced Keychain item — the collector half of
/// the m4 LEAVE ruling (2026-09-12: a session's item is
/// never cleared at teardown, so this sweep is what clears it). Goes through
/// the same salvage-then-delete core the census takes: any readable bytes are
/// quarantined first, a refused or prompted read (a foreign item's ACL, a
/// locked keychain) is named on the event line rather than read as success,
/// and the ownership row is retired only after the delete lands so a failed
/// collection stays collectible by a later census. Runs after the
/// state-flock closure (a `security` subprocess must never span it), inside
/// the shared subprocess budget [`gc_stale_runtimes`] arms, and is
/// loud-not-fatal: the item is inert without its dir, so a failed collection
/// leaves stale clutter rather than breaking anything. The delete sink takes
/// the in-flight witness the re-check's hold minted, and the record it proves
/// clears once the delete's outcome is known — landed or failed; only a crash
/// leaves it standing, for the staleness sweep.
#[cfg(target_os = "macos")]
fn collect_orphaned_keychain_item(in_flight: namespaced_keychain_ledger::InFlightDelete) {
    let service = in_flight.service().to_string();
    match crate::keychain::salvage_delete_namespaced_item(&in_flight) {
        Ok(salvage) => {
            let retirement = namespaced_keychain_ledger::retire(&service);
            let cleared = in_flight.clear();
            logline!(
                "clauth: collected the orphaned per-session Keychain item {service} (its runtime tree \
                 is gone); {}; {}; {}",
                crate::claude::salvage_tail(&salvage),
                crate::claude::retirement_tail(&retirement),
                crate::claude::in_flight_tail(&cleared)
            );
        }
        Err(e) => {
            let cleared = in_flight.clear();
            logline!(
                "clauth: collecting the orphaned per-session Keychain item {service} failed: {e:#}. It \
                 holds a login only the removed tree's dir resolved, so it stays inert in the Keychain \
                 until a later sweep or census removes it; {}",
                crate::claude::in_flight_tail(&cleared)
            );
        }
    }
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
/// dir backing it. Each session gets its own store under both link modes, so a
/// profile running two isolated sessions appears twice, once per store. A
/// consumer keying by profile name must expect that, and must not read the row
/// count as a session count.
///
/// An isolated runtime's transcripts live
/// ONLY in this throwaway tree (never symlinked to the global store) and are
/// rescued into the global store on teardown ([`crate::start::rescue_teardown`])
/// and GC ([`rescue_tombstone`]); this listing reaches them only while the
/// session is live. Gated on a live *isolated* session specifically (not
/// [`has_live_session`], which also counts shared sessions) and on the projects
/// dir existing, so a shared-only or not-yet-written runtime is skipped.
/// Fail-soft: an unreadable profiles root or entry is skipped, never an error.
///
/// Claude-only by roster, not by accident: the session index this feeds reads
/// Claude Code transcripts, and a codex profile's dir — which sits in the same
/// root — is skipped outright rather than relying on it never containing a
/// `runtime-isolated*` child.
pub(crate) fn live_isolated_stores() -> Vec<(String, PathBuf)> {
    let Ok(root) = profiles_root_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let codex = crate::codex_profiles::CodexState::load().unwrap_or_default();
    let claude_roster: Vec<String> = crate::profile::claude_roster_names()
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.to_string())
        .collect();
    let mut out = Vec::new();
    for profile in entries.flatten() {
        let profile_name = profile.file_name();
        let Some(profile_name) = profile_name.to_str() else {
            continue;
        };
        // Claude-first for a dual-claimed name, like every other site.
        if codex.holds(profile_name) && !claude_roster.iter().any(|p| p == profile_name) {
            continue;
        }
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
/// `validate_profile_name` bounds a name to [`crate::actions::is_name_char`]
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
/// one. Three legs have no bound at all: a sibling start's recursive `~/.claude`
/// copy inside [`build_runtime_dir_with_active_env`], the state-flock
/// acquisitions of the longest holder — `oauth::gate_under_guard`, which takes
/// up to four and whose count moves with its quarantine branches — and, on
/// macOS, a state flock a peer is legitimately holding across its own Keychain
/// budget. A sum over legs like those is a number wearing a proof's clothes.
///
/// So it is derived from the two ends that are fixed.
///
/// The FLOOR is the two legs a HEALTHY holder spends real time in, so an ordinary
/// rotation is waited out rather than refused:
/// - [`crate::oauth::TOKEN_HTTP_DEADLINES`] — every rotation makes exactly
///   one token call, and `timeout_global` holds that call — body included — to
///   the sum of the two deadlines it carries. The floor wants a number a healthy
///   call comfortably fits inside, which this is, and it is a ceiling on the
///   call itself.
/// - [`KEYCHAIN_MIRROR_BUDGET`] — a macOS rotation mirrors the new pair into the
///   Keychain and may spend that whole budget doing it: the landing arms one
///   [`crate::lock::SharedSubprocessBudget`] of it once its state-flock hold
///   ends, which every `security` call of the mirror shares. Never
///   `crate::lock::SUBPROCESS_BUDGET`, which the two coincide with today: that one
///   bounds a state-flock hold's shell-outs in aggregate, and the mirror runs
///   after the hold. Kept in the sum on every host, for the reason stated at the
///   constant.
/// - [`SESSION_SEED_BUDGET`] — a macOS `clauth start` seeds the session's
///   per-config-dir Keychain item under this same lock (past the state flock,
///   before the guard drops), spending one shared budget on the item write.
///   Same non-waste reasoning off macOS as the mirror term.
/// - [`crate::claude_json::SEED_SYNC_LOCK_BUDGET`] — a `clauth start` seeds
///   the runtime `.claude.json` copy under this same lock (past the state
///   flock, before the guard drops), waiting up to this healthy-merge budget
///   for `JsonSyncLock`. Spelled here for the same reason as the mirror and
///   seed terms: the deadline derivation is one number on every host.
///
/// Each guard holder's own legs, on the terms above. Every budget below is armed
/// past every state-flock wait before the `security` calls it bounds, so
/// neither a flock wait nor a token call spends it; a stage landing (`oauth::land_pair`,
/// the rotation persist included) is the flock wait, the store write, then its
/// mirror under one [`KEYCHAIN_MIRROR_BUDGET`]. An HTTP leg counts by its
/// deadlines, as the token call does: an identity probe (`/api/oauth/profile`
/// on the usage agent) by its connect plus header deadlines, 4 + 8 = 12 s, which
/// each agent's whole-call deadline also holds its body to, so an HTTP term is a
/// ceiling on its call. A figure below stays a budget of the legs it names, never
/// a ceiling on the holder: the state-flock waits sit outside every one. Every
/// spend resolves the profile's stage first (`oauth::send_rule`), under the same
/// hold and before its token call: one more stage landing, so one more
/// [`KEYCHAIN_MIRROR_BUDGET`] wherever a spend runs.
/// - a rotation (`oauth::rotate_one_inner`, the kick, the vanilla install
///   gate): the pre-spend stage landing, the token call, then its persist's
///   landing, `KEYCHAIN_MIRROR_BUDGET + TOKEN_HTTP_DEADLINES +
///   KEYCHAIN_MIRROR_BUDGET` = 20 + 19 + 20 = 59 s;
/// - the scheduler's fetch leg, on the active profile, the longest holder
///   ([`FETCH_LEG_HOLD`]): before the token call,
///   `oauth::try_adopt_live_rotation` reads the live credentials FILE (no
///   Keychain call) and probes identity twice at most (the stored token, only
///   with no account id cached, and the live one), each at
///   `usage::USAGE_HTTP_DEADLINES`, then the pre-spend stage landing, then the
///   token call, then either the persist's landing or, on a failed refresh, a
///   second adopt whose probes the memo answers save a live token that moved
///   meanwhile: 24 + 20 + 19 + 20 = 83 s; 12 + 20 + 19 + 20 = 71 s with the
///   account id cached;
/// - [`ProfileRuntime::acquire_adopted`]: the stage landing's budget, then the
///   store write and relink (both sub-millisecond disk work inside the
///   already-held state flock) =
///   20 s;
/// - [`ProfileRuntime::acquire`]: the landing's budget, the macOS seed
///   carry's item read at `keychain::SECURITY_TIMEOUT`, then the seed write's
///   own [`SESSION_SEED_BUDGET`], then the `.claude.json` seed's sync-lock
///   wait ([`crate::claude_json::SEED_SYNC_LOCK_BUDGET`]) =
///   20 + 10 + 20 + 10 = 60 s; a seed signing out a login-less store's item
///   instead carries nothing and runs its sign-out under that same
///   [`SESSION_SEED_BUDGET`], 20 + 20 + 10 = 50 s;
/// - `SessionSwap::swap_to` onto another member: the landing's budget, the
///   macOS carry's item read at `keychain::SECURITY_TIMEOUT`, then one
///   [`KEYCHAIN_MIRROR_BUDGET`] over the item write or sign-out =
///   20 + 10 + 20 = 50 s;
/// - `SessionSwap::converge_in_place` (a same-member move onto an armed
///   sidecar; no landing): the macOS carry's item read at
///   `keychain::SECURITY_TIMEOUT`, then one [`KEYCHAIN_MIRROR_BUDGET`] over
///   the sign-out = 10 + 20 = 30 s;
/// - `oauth::rolling_install_gate` (the rolling arm, the re-stamp leg, a
///   rolling switch target): the stage landing's budget, the token call, then
///   the refresh persist's landing budget = 59 s; its spend's own pre-spend
///   resolve finds no stage, since the gate's landing already deleted it under
///   the same hold;
/// - the force-link holders (`static-token` clear and restore, the TUI's clear,
///   the start-time rolling arm), the install gate's lander and the catch-up:
///   the landing's budget = 20 s, plus a relink's own state-flock hold, which
///   [`crate::lock::SUBPROCESS_BUDGET`] caps at 20 s = 40 s.
///
/// The deadline is the longer of the floor's sum and the longest holder's own
/// legs ([`FETCH_LEG_HOLD`]): 83 s against the floor's 19 + 20 + 20 + 10 = 69 s
/// today, so a start queued behind any one holder above waits it out.
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
/// The CEILING is Claude Code's 30-minute stdio idle abort: the MCP
/// `delegate`'s pre-spawn window emits no progress notification, there being
/// no child to report on yet, so the wait sits inside whatever silence the
/// host tolerates before aborting the call. Past the abort the named refusal
/// below reaches nobody. Pinned as a relation rather than restated here.
///
/// A holder past this deadline gets a named retry rather than a fault, because
/// the unbounded legs mean a firing is not proof of a wedge.
///
/// `saturating_add` over `as_secs()` arithmetic: every term is whole seconds
/// today, and a sub-second one added later would round DOWN through `as_secs`,
/// quietly shortening the deadline it was meant to lengthen.
pub(crate) const ROTATION_LOCK_TIMEOUT: Duration = longer(
    crate::oauth::TOKEN_HTTP_DEADLINES
        .saturating_add(KEYCHAIN_MIRROR_BUDGET)
        .saturating_add(SESSION_SEED_BUDGET)
        .saturating_add(crate::claude_json::SEED_SYNC_LOCK_BUDGET),
    FETCH_LEG_HOLD,
);

/// The scheduler's fetch-leg rotation of the active profile under its rotation
/// guard, the longest holder [`ROTATION_LOCK_TIMEOUT`]'s list derives: two
/// identity probes, the pre-spend stage landing, the token call, then the
/// persist's landing.
const FETCH_LEG_HOLD: Duration = crate::usage::USAGE_HTTP_DEADLINES
    .saturating_add(crate::usage::USAGE_HTTP_DEADLINES)
    .saturating_add(KEYCHAIN_MIRROR_BUDGET)
    .saturating_add(crate::oauth::TOKEN_HTTP_DEADLINES)
    .saturating_add(KEYCHAIN_MIRROR_BUDGET);

/// The longer of two durations, in a `const` context.
const fn longer(a: Duration, b: Duration) -> Duration {
    if a.as_nanos() >= b.as_nanos() { a } else { b }
}

/// What a macOS landing's Keychain mirror is budgeted for under the rotation
/// lock: two `security` invocations at `keychain::SECURITY_TIMEOUT` each. A
/// landing that mirrors makes up to FOUR: the foreign gate's item read
/// (`keychain::item_login_state`), then the mirror's merge read, write and
/// read-back verify. Every landing (`oauth::land_pair`, the rotation persist
/// included) arms one [`crate::lock::SharedSubprocessBudget`] of this size, or
/// adopts the daemon tick's window, once its state-flock hold ends, so the four
/// share it and each later call is clamped to what the earlier ones left.
/// `SessionSwap::swap_to` arms one more over its item write or sign-out, and
/// `SessionSwap::converge_in_place`, which lands nothing, arms one over its
/// sign-out (three calls at most), each past the carry's store write and the
/// ownership row.
/// `keychain::SECURITY_TIMEOUT`'s doc carries the
/// per-call derivation.
///
/// Spelled here rather than read out of `keychain`, which is macOS-gated while
/// this deadline is one number on every host. Off macOS the term is not waste: it
/// is the headroom the only slow leg a holder has there — the token call,
/// which the other term derives for — would otherwise have none of. `keychain` holds
/// the other side of the derivation as a `const` assertion, so a re-tune of
/// `SECURITY_TIMEOUT` fails to COMPILE on the platform that has one.
pub(crate) const KEYCHAIN_MIRROR_BUDGET: Duration = Duration::from_secs(20);

/// What the macOS session-start Keychain seed is budgeted for under the
/// rotation lock: one [`crate::lock::SharedSubprocessBudget`] of this size
/// over the item write, or the sign-out a login-less store takes instead,
/// armed past the carry's store write and the ownership row so neither flock
/// wait spends it ([`owned_keychain_leg`]), so a stuck keychain (an
/// unanswered ACL dialog, a locked keychain) cannot spend more than this
/// before the leg starts refusing rather than hanging. Three `security`
/// invocations worst case (the write's merge read, put and verify; the
/// sign-out's read, then a put and verify or a delete), each capped by
/// `keychain::SECURITY_TIMEOUT`; the shared budget is
/// the aggregate bound the per-call cap cannot supply, the same discipline
/// [`KEYCHAIN_MIRROR_BUDGET`] holds the rotation mirror to. The carry's item
/// read runs before the arm, on its own per-call cap.
///
/// Spelled here rather than read out of `keychain`, same reason as
/// [`KEYCHAIN_MIRROR_BUDGET`]: the deadline derivation is one number on every
/// host. Off macOS the term is not waste — it is headroom for the seed leg
/// the same way the mirror budget is.
///
/// In AGGREGATE a healthy holder spends ~none of it (13-29 ms per
/// `security` call, measured in `keychain.rs`); the budget only ever binds on
/// a stuck keychain, where the alternative is an unbounded hold under the
/// rotation guard.
pub(crate) const SESSION_SEED_BUDGET: Duration = Duration::from_secs(20);

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
            "timed out after {:.0}s taking '{}' rotation lock. A token rotation or another \
             session start is holding it. Retry, or start a different account",
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
                    "waiting for {display} ended with no verdict. The thread holding the \
                     wait died. Retry the command"
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

/// Why this host cannot execute a per-session credential swap. The arm is
/// structural rather than unfinished work, and must REFUSE loudly: a swap that
/// silently leaves the session on its launch account is the one outcome the
/// live-Claude-Code probe exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapUnsupported {
    /// [`LinkMode::Fake`] shares ONE runtime tree across every SHARED session of
    /// the profile, so repointing its credential file would move every such
    /// session at once.
    SharedRuntimeTree,
}

impl std::fmt::Display for SwapUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SharedRuntimeTree => {
                f.write_str("this host shares one runtime tree across the profile's sessions")
            }
        }
    }
}

/// The transport gate, kept PURE so the refusal is exercised from any test run.
fn swap_support(mode: LinkMode) -> Result<(), SwapUnsupported> {
    if mode == LinkMode::Fake {
        return Err(SwapUnsupported::SharedRuntimeTree);
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
fn chain_opt_in_survives(requested: bool, isolation: Isolation, mode: LinkMode) -> bool {
    requested && isolation == Isolation::Shared && swap_support(mode).is_ok()
}

/// Probe `name`'s profile dir for the [`LinkMode`] the session's own build
/// resolves, exactly as [`ProfileRuntime::acquire`] does: create the profile
/// dir, then [`detect_link_mode`] under the state lock, so two concurrent
/// starts cannot interleave their probe dotfiles and read a spurious
/// [`LinkMode::Fake`]. An IO failure propagates rather than reading as either
/// answer — a probe that could not run says nothing about the host.
fn probe_profile_link_mode(name: &ProfileName) -> Result<LinkMode> {
    let profile_root = profile_dir(name)?;
    with_state_lock(|_held| {
        crate::profile::mkdir_700(&profile_root)
            .with_context(|| format!("failed to create {}", profile_root.display()))?;
        detect_link_mode(&profile_root)
    })
}

/// [`swap_support`]'s TRANSPORT arm, which is only knowable by probing. Run LAST
/// among the `--with-fallback` gates: it is the one leg that writes, so a start
/// refused for any other cause never materializes a profile dir for an account
/// that never launched.
///
/// `Ok(None)` is the supported host.
pub(crate) fn unsupported_swap_transport(name: &ProfileName) -> Result<Option<SwapUnsupported>> {
    Ok(swap_support(probe_profile_link_mode(name)?).err())
}

/// Whether `name`'s runtime trees are COPIED rather than linked —
/// [`LinkMode::Fake`], resolved the way [`ProfileRuntime::acquire_background`]
/// resolves it ([`probe_profile_link_mode`]). A background launch on a copy
/// host would build the fixed pair as a copy, which cannot follow a rotation
/// that relinks the credentials a background session holds — the session would
/// outlive them — so [`crate::start::admit`] refuses it before anything
/// spawns.
pub(crate) fn background_copy_host(name: &ProfileName) -> Result<bool> {
    Ok(probe_profile_link_mode(name)? == LinkMode::Fake)
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
    /// A background holder: its tree is the account's fixed pair, shared by
    /// every Claude Code job on that account, so a move onto another member
    /// would bill another account under a running job. Only a cross-account
    /// target is refused; the holder's own account convergence still runs.
    FixedAccount,
    ProfileUnreadable(String),
    /// The outgoing member's spent-token record cannot be read, so the session's
    /// own credential file cannot be judged and nothing moved. Names the
    /// outgoing member, not the intended one.
    OutgoingRecordUnreadable(String),
    /// Carries a `base_url`: a different endpoint, not the same account elsewhere.
    NotOauth,
    Disabled,
    /// `settings.json` env reaches Claude Code's `process.env` only at STARTUP,
    /// so a member with different env is a genuinely different transport.
    EnvDiffers,
    ModelsDiffers,
    ApiKeyDiffers,
    /// No login to swap onto: nothing at `install_source_path`, or a store the
    /// re-read proves holds none ([`crate::profile::StoredLogin::HoldsNone`]).
    NoCredentialStore,
    /// The intended member's stored login exists but cannot be read or parsed
    /// ([`crate::profile::StoredLogin::Unknown`]): the swap refuses rather than
    /// link a guess. Distinct from [`Self::NoCredentialStore`], which names
    /// nothing at `install_source_path` or a store the re-read proves holds none
    /// — a torn sidecar is unreadable, not absent.
    LoginUnreadable,
    /// A live process holds the marker this session would need on the intended
    /// member — a colliding session id. Claiming it anyway would leave two
    /// sessions sharing one marker identity, and teardown unlinks only what it
    /// owns, so the survivor would be reported dead while it runs.
    MarkerNotLockable,
    /// A same-member convergence's macOS carry-back of the per-session
    /// Keychain item failed with a non-transient class. The class keys the
    /// once-per-(member, reason) announcement; the classified locked-keychain
    /// transient never arrives here ([`converge_leg_disposition`] stops it
    /// silently).
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "constructed only by the macOS convergence legs; the memo pins construct it on every platform"
        )
    )]
    ConvergeCarryFailed(crate::claude::SecurityExitClass),
    /// The convergence's macOS sign-out leg failed with a non-transient class
    /// — the twin of [`Self::ConvergeCarryFailed`] so a carry failure and a
    /// sign-out failure stay distinct reasons to the announcement memo.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "constructed only by the macOS convergence legs; the memo pins construct it on every platform"
        )
    )]
    ConvergeSignOutFailed(crate::claude::SecurityExitClass),
}

impl std::fmt::Display for SwapRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(why) => write!(f, "{why}"),
            Self::ShuttingDown => f.write_str("the session is shutting down"),
            Self::AlreadyCurrent => f.write_str("the link already resolves to it"),
            Self::IsolatedSession => f.write_str("an isolated session follows no chain"),
            Self::FixedAccount => {
                f.write_str("a background session stays on its account's fixed tree")
            }
            Self::ProfileUnreadable(e) => write!(f, "its profile could not be read: {e}"),
            Self::OutgoingRecordUnreadable(member) => write!(
                f,
                "cannot read {member}'s spent-token record, so the session keeps its own credential copy"
            ),
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
            Self::LoginUnreadable => f.write_str(crate::format::STORED_LOGIN_UNREADABLE),
            Self::MarkerNotLockable => {
                f.write_str("its liveness marker is held by another process")
            }
            Self::ConvergeCarryFailed(class) => write!(
                f,
                "carrying its per-session Keychain pair back failed ({class:?})"
            ),
            Self::ConvergeSignOutFailed(class) => write!(
                f,
                "signing its per-session Keychain item out failed ({class:?})"
            ),
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
///
/// The `[env]` map lives here only as a digest: an env VALUE may carry an
/// inference credential, so the map itself must not travel onto the live-session
/// row. [`swap_eligible`] compares the candidate's digest against this one, so
/// the verdict equals a compare of the two maps. The row persists it across
/// upgrades, so its encoding is a format (see `env_digest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaunchTransport {
    env_digest: String,
    models: crate::profile::ModelSettings,
    has_api_key: bool,
}

impl LaunchTransport {
    pub(crate) fn of(profile: &Profile) -> Self {
        Self {
            env_digest: env_digest(&profile.env),
            models: profile.models.clone(),
            has_api_key: profile.api_key.is_some(),
        }
    }
}

/// The SHA-256 of the sorted `[env]` map, lower-case hex. Each key and value is
/// length-prefixed (`u64` little-endian byte count then bytes), and the pair
/// count leads, so a pair split at a different `=` cannot collide:
/// `{"A": "b=c"}` and `{"A=b": "c"}` digest differently, and reordered inserts
/// digest identically. The ONE routine both the boot snapshot and
/// [`swap_eligible`] use — there is deliberately no second digest routine.
/// The bytes stream into the hasher, so no heap copy of a value is made: a
/// value may be an inference token.
///
/// The value is PERSISTED on the live-session row, and a row outlives a binary
/// upgrade: a newer daemon judges a session an older build started. Changing
/// this encoding makes every pre-upgrade row mismatch every member until its
/// session restarts, so treat it as a format.
fn env_digest(env: &std::collections::BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update((env.len() as u64).to_le_bytes());
    for (key, value) in env {
        crate::usage::hash_field(&mut hasher, key.as_bytes());
        crate::usage::hash_field(&mut hasher, value.as_bytes());
    }
    hex::encode(<[u8; 32]>::from(hasher.finalize()))
}

/// Whether `candidate` is a member this session could be swapped onto, judged on
/// CONFIG grounds alone — no disk IO, no platform or transport-mode check.
///
/// Shared with the daemon's per-session decision leg, which needs it as a walk
/// PREFERENCE: a candidate the executor refuses is one the walk must step PAST,
/// or the intent stops changing and the session never reaches the next viable
/// member (a chain holding a `z.ai` or DeepSeek profile is enough to do it). One
/// function rather than two so the preference and the gate cannot drift. The leg
/// passes the boot transport its row records — the same snapshot the executor
/// holds — so for a session this clauth launched they judge one transport; an
/// older row's reconstruction is the leg's fallback.
///
/// The executor stays the safety gate. What is deliberately NOT here: the
/// store-exists check (disk IO the daemon would repeat per candidate per tick)
/// and the transport/platform + isolated refusals ([`swap_support`], which the
/// decision leg has no business re-deriving).
///
/// `models` relaxes ONLY the `[models]`-equality arm: a credit-cap leave (the
/// session's own 429, or a credit-billed launch on a capped account) may land
/// on a member whose `[models]` differ, since a swap moves credentials only and
/// the session keeps running its launch model on the new member. Every other
/// leave keeps the models-equal rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelsRule {
    Equal,
    Any,
}

pub(crate) fn swap_eligible(
    candidate: &Profile,
    launch: &LaunchTransport,
    models: ModelsRule,
) -> Result<(), SwapRefused> {
    if !candidate.is_oauth() {
        return Err(SwapRefused::NotOauth);
    }
    if candidate.is_disabled() {
        return Err(SwapRefused::Disabled);
    }
    if env_digest(&candidate.env) != launch.env_digest {
        return Err(SwapRefused::EnvDiffers);
    }
    if matches!(models, ModelsRule::Equal) && candidate.models != launch.models {
        return Err(SwapRefused::ModelsDiffers);
    }
    if candidate.api_key.is_some() != launch.has_api_key {
        return Err(SwapRefused::ApiKeyDiffers);
    }
    Ok(())
}

/// The target-member half of [`SessionSwap::precondition`]'s install branch:
/// whether a member the session is NOT already on can be swapped onto, judged on
/// the SAME checks the executor runs, in the SAME order — the `stored_login`
/// re-read BEFORE the profile read, then the side-effect-free
/// [`crate::profile::read_profile`], [`swap_eligible`],
/// [`crate::claude::install_source_path`] and the store-exists check.
///
/// The profile read is deliberately NOT [`crate::profile::load_profile`]: this
/// answers a read-only question, so it takes no state flock and writes nothing.
/// The load's recovery/rewrite legs (a legacy `.pending` adoption, a
/// `config.toml` canonical rewrite) are the executor's own cleanup, run by
/// [`SessionSwap::precondition`] after this verdict passes.
///
/// Shared with the daemon's per-session decision leg so it steps past EXACTLY
/// what the executor refuses, rather than a second copy of the gate that drifts
/// the next time the executor gains a check. For a session this clauth launched,
/// the leg passes the same boot transport the executor holds. Returns the
/// install source path the caller's [`SwapPlan`] repoints the link at.
pub(crate) fn swap_target_precondition(
    intended: &ProfileName,
    launch: &LaunchTransport,
    models: ModelsRule,
) -> Result<PathBuf, SwapRefused> {
    // The login re-read runs BEFORE the read: a member whose store reads
    // `Unknown` (a torn `credentials.json`, a torn sidecar, a key member's torn
    // `config.toml`) refuses `LoginUnreadable` and no parse-error text reaches
    // the line; a proven-none store refuses `NoCredentialStore`. Only a login
    // that read fine reaches the profile read, so `ProfileUnreadable` keeps its
    // real cause: a profile-read or install-source failure past a readable
    // login.
    match crate::profile::stored_login(intended) {
        crate::profile::StoredLogin::HoldsLogin => {}
        crate::profile::StoredLogin::HoldsNone => {
            return Err(SwapRefused::NoCredentialStore);
        }
        crate::profile::StoredLogin::Unknown => {
            return Err(SwapRefused::LoginUnreadable);
        }
    }
    let profile = crate::profile::read_profile(intended)
        .map_err(|e| SwapRefused::ProfileUnreadable(format!("{e:#}")))?;
    swap_eligible(&profile, launch, models)?;
    let store = crate::claude::install_source_path(intended)
        .map_err(|e| SwapRefused::ProfileUnreadable(format!("{e:#}")))?;
    if !store.exists() {
        return Err(SwapRefused::NoCredentialStore);
    }
    Ok(store)
}

/// A precondition-cleared swap target, minted ONLY by
/// [`SessionSwap::precondition`]. The install branch runs `profile::load_profile`
/// past the shared check, and THAT load is what adopts-or-discards a legacy
/// `credentials.json.pending` sidecar and then removes it; the converge branch
/// (a same-member move) mints its plan with no load, because its store is the
/// refreshless sidecar ([`crate::claude::install_source_path`]) and no
/// `.pending` is at risk there.
///
/// [`touch_store`] takes one of these as its argument for exactly that reason:
/// the stamp it leaves is only readable as a non-write for as long as its receipt
/// stands, so the sidecar is still resolved against real bytes first.
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
    stamp_store(&plan.member, &plan.store, memoized)
}

/// [`touch_store`]'s stamp, keyed on the store alone. Reached by that first
/// stamp, and by the re-stamp after a swap's item leg lands
/// ([`SessionSwap::retouch_after_item_leg`]), which only follows a stamp a
/// [`SwapPlan`] already keyed: the `.pending` resolution the plan stands for
/// ran before that first stamp, and the receipt it left resolves the
/// re-stamp's displaced value back to the store's real write time.
fn stamp_store(member: &ProfileName, store: &Path, memoized: Option<SystemTime>) -> Result<()> {
    // Through the resolver, not the raw mtime: on a swap BACK onto a member the
    // value being displaced is that member's own earlier stamp, and a receipt
    // recording a stamp as a write time hands the readers the exact answer this
    // exists to prevent — eroding by one stamp per revisit, on the chain churn
    // the feature is for.
    let displaced = crate::profile_cache::effective_write_time(store);
    let file = OpenOptions::new()
        .write(true)
        .open(store)
        .with_context(|| format!("failed to open {}", store.display()))?;
    let stamp = |at: SystemTime| {
        file.set_times(std::fs::FileTimes::new().set_modified(at))
            .with_context(|| format!("failed to touch {}", store.display()))
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
        crate::profile_cache::write_touch_receipt(member, store, stamped, displaced);
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

// Gated with its caller: the one test that poses a truncating filesystem
// drives `swap_to`, whose mtime-touch legs need the override. The static stays
// ungated — `as_stored` reads it on every platform.
#[cfg(test)]
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

/// One swapped-onto member's liveness marker, held for the session's life.
struct SwappedMarkers {
    pid_file: PathBuf,
    pid_lock: File,
}

/// One marker an adoption claimed, with its paths' state taken before the
/// claim touched them: `created_file`/`created_dir` are true only where THIS
/// attempt made the path, so a rollback never removes a marker another holder
/// made or a `sessions-<sid>/` dir that stood before the attempt.
struct AdoptionMarkerPaths {
    pid_file: PathBuf,
    sessions: PathBuf,
    created_file: bool,
    created_dir: bool,
}

/// The markers an adoption has claimed while its refusals still run, in claim
/// order. Dropped uncommitted it takes back exactly what those claims created;
/// committed, the hold keeps them for the session's life.
///
/// The caller declares this guard before the [`SwappedMarkers`] it collects, so
/// those release their flocks first and the unlink never meets its own handle.
#[derive(Default)]
struct AdoptionClaims {
    made: Vec<AdoptionMarkerPaths>,
    committed: bool,
}

impl AdoptionClaims {
    /// Snapshot a marker's paths before a claim touches them. Pushed ahead of
    /// the claim, so a claim that fails partway is covered too.
    fn begin(&mut self, paths: &SessionPaths) {
        let existed = |path: &Path| std::fs::symlink_metadata(path).is_ok();
        self.made.push(AdoptionMarkerPaths {
            pid_file: paths.pid_file.clone(),
            sessions: paths.sessions.clone(),
            created_file: !existed(&paths.pid_file),
            created_dir: !existed(&paths.sessions),
        });
    }

    /// The claim sequence finished: the hold owns every marker and the files
    /// stay.
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for AdoptionClaims {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for made in self.made.drain(..).rev() {
            if made.created_file {
                let _ = std::fs::remove_file(&made.pid_file);
            }
            if made.created_dir {
                // Fails when a sibling has put a marker in it since, which is
                // exactly when the dir is not this attempt's to remove.
                let _ = std::fs::remove_dir(&made.sessions);
            }
        }
    }
}

/// What claiming a member's liveness marker found.
enum MarkerClaim {
    /// Freshly stamped; hold for the session's life.
    Stamped(SwappedMarkers),
    /// This session already holds it — a member it has run on before. `flock`
    /// locks the open file description, so a second `open` + `try_lock` from THIS
    /// process is denied by our own lock; without recognizing that, every swap back
    /// onto a recovered member would read as a foreign holder and be refused,
    /// which removes the chain's whole recovery half.
    AlreadyOurs,
    /// A live process that is not this one holds it.
    Foreign,
}

/// Stamp and hold the intended member's per-session liveness marker.
///
/// It makes the swapped-onto member read live to [`has_live_session`], which is
/// what stops a delete or disable landing on the account this session is now
/// running as, and on macOS what [`rotation_blocked_for`] reads to refuse a
/// rotation of it — for a FOREGROUND session. A per-session tree a daemon
/// adopts is a background hold: the rotation gate skips a background row's
/// markers, refuses only the account owning the store the row's `launch_store`
/// names (its start, current and store-path accounts when that store is absent
/// or no longer resolves, see [`job_block`]), and so leaves the accounts the
/// session has moved off rotatable.
/// The marker still keeps each of them live for delete, disable and rename.
///
/// `None` when the marker is held by something else, so the caller
/// refuses the swap. A foreign holder means a colliding session id, and marker
/// ownership is what teardown keys on — unlinking a marker another session minted
/// would report that session dead while it runs. The swap path comes through
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
    Ok(Some(SwappedMarkers {
        pid_file: paths.pid_file.clone(),
        pid_lock,
    }))
}

/// The member a live session's credential link resolves to, and the markers of
/// every member it has run on.
struct SwapCell {
    member: String,
    canonical: PathBuf,
    /// Markers stamped by swaps, in visit order. NEVER released mid-session: the
    /// live Claude Code child still holds every refresh token it has been handed
    /// and nothing can observe when it stops using one. So for a FOREGROUND
    /// session every marker here refuses a rotation of its account on macOS
    /// while the session's store holds a refresh token, through
    /// [`rotation_blocked_for`]'s marker term.
    ///
    /// A per-session tree the daemon adopts is a BACKGROUND hold: that term
    /// skips every row whose `background_hold` is set, so these markers do no
    /// rotation work there. The job term decides instead, refusing only the
    /// account owning the store the row's `launch_store` names (its start,
    /// current and store-path accounts when that store is absent or no longer
    /// resolves, see [`job_block`]); the accounts
    /// the session has moved off stay rotatable. Every marker here still keeps
    /// its account live for delete, disable and rename.
    held: Vec<SwappedMarkers>,
    /// The last refusal announced, so a stuck `intended_member` states its reason
    /// once instead of once per watchdog tick.
    last_refusal: Option<(String, SwapRefused)>,
    /// The item legs a swap left unlanded on a transient, for the swap poll to
    /// re-run ([`SessionSwap::retry_item_leg_with`]). Kept beside the member
    /// and store it was armed against, so the staleness check and the clear on
    /// a later [`publish_swap`](SessionSwap::publish_swap) read one cell.
    item_retry: Option<ArmedItemRetry>,
    /// The last reason announced for an item retry that could not start (an
    /// unopenable rotation lock, a stage landing refused), so a stuck retry
    /// states it once instead of once per poll.
    item_retry_stall: Option<(ProfileName, String)>,
}

/// A swap's per-session Keychain item legs that failed on a transient: the
/// member swapped onto, its install source the legs install or hand off
/// (`store`), and the outgoing member's store the carry writes the item's
/// pair back into (`previous_store`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "armed only by the macOS swap's item legs; the retry state machine is pinned on every platform"
    )
)]
struct SwapItemRetry {
    member: ProfileName,
    store: PathBuf,
    previous_store: PathBuf,
}

/// A [`SwapItemRetry`] in the cell, with its backoff: the attempt it next
/// runs no earlier than (`due`), and the wait that set it (`gap`, zero while
/// no attempt has run since arming). A keychain that stays locked would
/// otherwise cost a `security` read per session on every swap poll.
#[derive(Debug)]
struct ArmedItemRetry {
    target: SwapItemRetry,
    due: Instant,
    gap: Duration,
}

/// The longest wait between two item-retry attempts: a keychain unlocked
/// mid-backoff is picked up within this.
const ITEM_RETRY_MAX_GAP: Duration = Duration::from_secs(30);

/// The wait before the next item-retry attempt, given the last one: one
/// second after the first attempt since arming, then doubling, capped at
/// [`ITEM_RETRY_MAX_GAP`].
fn next_item_retry_gap(prev: Duration) -> Duration {
    if prev.is_zero() {
        Duration::from_secs(1)
    } else {
        prev.saturating_mul(2).min(ITEM_RETRY_MAX_GAP)
    }
}

/// One attempt of a swap's item legs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "produced only by the macOS item legs; the retry state machine is pinned on every platform"
    )
)]
enum SwapItemLeg {
    /// The item holds the incoming login, or is handed off to the file.
    Landed,
    /// The item is untouched on a failure that clears on its own (a locked
    /// keychain, a sweep's in-flight delete): the next poll tries again.
    Transient,
    /// Any other failure: the retry drops.
    Failed,
}

/// The store a swap's carry-back writes the session's Keychain item into:
/// the store the cell names, unless an item retry is armed. Then the item still
/// holds the login of the retry's outgoing store (its legs never landed), so
/// carrying it into the cell's store would hand that member another member's
/// login.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS swap carry; the choice is pinned on every platform"
    )
)]
fn item_carry_target<'a>(armed_previous_store: Option<&'a Path>, canonical: &'a Path) -> &'a Path {
    armed_previous_store.unwrap_or(canonical)
}

/// The item retry an adopted session re-arms. On macOS a swap moves the
/// row's `launch_store` onto the incoming store only once its item legs land,
/// so a row whose `launch_store` (in its identity form) still differs from
/// the store the link resolves to records legs the launcher left unlanded,
/// and the adopting executor picks them up where the launcher's died. The
/// retry's own gates (staleness, try-lock, stage landing, the arm re-chosen
/// from the store) decide the rest.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS adoption; the decision is pinned on every platform"
    )
)]
fn adopted_item_retry(
    launch_store: Option<&Path>,
    canonical: &Path,
    member: &ProfileName,
) -> Option<SwapItemRetry> {
    let previous_store = launch_store.filter(|store| *store != canonical)?;
    Some(SwapItemRetry {
        member: member.clone(),
        store: canonical.to_path_buf(),
        previous_store: previous_store.to_path_buf(),
    })
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

/// Whether the same-member source transition the convergence admits holds:
/// the canonical the cell names parses refreshable while the profile's
/// install source now selects a DISTINCT refreshless store. Unknown and
/// unreadable inputs fail the predicate — do nothing, and the existing
/// fail-closed rotation refusal stays in place.
fn converge_transition_holds(current: &Path, selected: &Path) -> bool {
    if current == selected {
        return false;
    }
    let refreshable = crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(current)
        .ok()
        .is_some_and(|c| c.refresh_token().is_some());
    let refreshless = crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(selected)
        .ok()
        .is_some_and(|c| c.refresh_token().is_none());
    refreshable && refreshless
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
    /// The pair this session occupies. A background holder resolves every
    /// per-member path through it, so no executor leg can stamp a
    /// `sessions-<sid>/` beside the fixed `sessions-bg/`.
    tree: SessionTree,
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
    /// Ticks this session's watchdog reconciled on — the fallback cadence or the
    /// polling fallback — rather than a filesystem event. Test-only observable:
    /// the event-leg pins count these to forbid the tick leg without a wall-clock
    /// bound. Compiled out of production builds.
    #[cfg(test)]
    tick_reconciles: std::sync::atomic::AtomicU64,
}

impl SessionSwap {
    /// `paths` is the LAUNCH member's resolved paths, so the runtime dir and the
    /// marker this session already holds come from one source and cannot disagree.
    fn new(
        session: SessionId,
        isolation: Isolation,
        mode: LinkMode,
        tree: SessionTree,
        launch: &Profile,
        canonical: PathBuf,
        paths: &SessionPaths,
    ) -> Self {
        Self::from_parts(
            session,
            isolation,
            mode,
            tree,
            LaunchTransport::of(launch),
            launch.name.as_str().to_string(),
            canonical,
            paths.runtime.clone(),
            paths.pid_file.clone(),
            Vec::new(),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one part per SessionSwap field; the split keeps new and the adoption sharing it"
    )]
    fn from_parts(
        session: SessionId,
        isolation: Isolation,
        mode: LinkMode,
        tree: SessionTree,
        launch: LaunchTransport,
        member: String,
        canonical: PathBuf,
        runtime: PathBuf,
        marker: PathBuf,
        held: Vec<SwappedMarkers>,
    ) -> Self {
        Self {
            session,
            isolation,
            mode,
            tree,
            runtime,
            launch,
            launch_marker: marker,
            cell: crate::lockorder::RankedMutex::new(SwapCell {
                member,
                canonical,
                held,
                last_refusal: None,
                item_retry: None,
                item_retry_stall: None,
            }),
            shutdown: ShutdownFlag::new(),
            #[cfg(test)]
            tick_reconciles: std::sync::atomic::AtomicU64::new(0),
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

    /// Ticks this session's watchdog reconciled on rather than on a filesystem
    /// event. Test-only: the event-leg pin reads this. Gated with its caller
    /// (the unix-only relogin test), so no dead-code red on the windows leg.
    #[cfg(all(test, unix))]
    fn tick_reconciles(&self) -> u64 {
        self.tick_reconciles
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The paths one member of this session's chain addresses, through the pair
    /// this session actually occupies. A background holder resolves the fixed
    /// pair, never a per-session dir the background layout does not have.
    fn paths_for(&self, member: &ProfileName) -> Result<SessionPaths> {
        resolve_tree(member, self.isolation, self.tree, &self.session, self.mode)
    }

    /// Claim `paths`'s marker for this session, separating a marker this session
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
    /// The daemon writes `intended_member` only for a row whose `follows_chain`
    /// is set (`clauth start --with-fallback` requests that), and `clauth
    /// switch <sid> <profile>` writes one for any live claude row but a
    /// background one, so a plain `start` session no writer has targeted polls
    /// and finds nothing to do.
    fn poll(&self) {
        let Some(row) = crate::live_sessions::get(self.session.as_str()) else {
            self.poll_converge();
            return;
        };
        let Some(intended) = row
            .intended_member
            .filter(|intended| *intended != self.member())
        else {
            self.poll_converge();
            return;
        };
        // A background holder is fixed on its ACCOUNT: the tree it reads is the
        // one pair every Claude Code job on that account shares, so a move would
        // bill another account under a running job. Its own account's
        // convergence is not a move and still runs below — a hold adopted
        // before the account was armed for rolling tokens would otherwise keep
        // the rotating pair for its whole life. `precondition` refuses the same
        // tail as the last line of defence.
        if self.tree.is_background() {
            self.announce_refusal(&intended, SwapRefused::FixedAccount);
            self.poll_converge();
            return;
        }
        // A credit-cap leave may land on a member whose `[models]` differ; the
        // daemon records that leave reason on the row beside the intent. Every
        // other move keeps the models-equal rule.
        let models = if row.credit_leave {
            ModelsRule::Any
        } else {
            ModelsRule::Equal
        };
        let sid = self.session.as_str();
        match self.swap_to_with(&intended, models) {
            Ok(SwapOutcome::Swapped) => {
                logline!("clauth: session {sid} swapped onto {intended}");
            }
            Ok(SwapOutcome::Refused(why)) => self.announce_refusal(&intended, why),
            Err(e) => logline!("clauth: session {sid} could not swap onto {intended}: {e:#}"),
        }
    }

    /// The in-place convergence leg: with no cross-member intent standing,
    /// detect whether this session's own member was armed for rolling tokens
    /// under it — the canonical still refreshable while the install source now
    /// selects a refreshless sidecar — and move the session onto the sidecar
    /// through the same executor. Silent on a refusal: the no-op, the
    /// unreadable inputs, and the locked-keychain transient are the steady
    /// state the next poll re-detects, and the fail-closed rotation refusal
    /// stands until the row's `launch_store` follows the link.
    ///
    /// Waits while an item retry is armed: the item still holds the outgoing
    /// member's login, which the convergence's carry would write into this
    /// member's store. The retry landing or dropping lets the next poll run it.
    fn poll_converge(&self) {
        if self.cell().item_retry.is_some() {
            return;
        }
        let member = self.member();
        match self.swap_to(&member) {
            Ok(SwapOutcome::Swapped) => {
                logline!(
                    "clauth: session {} converged onto {member}'s rolling token",
                    self.session.as_str()
                );
            }
            Ok(SwapOutcome::Refused(_)) => {}
            Err(e) => logline!(
                "clauth: session {} could not converge onto {member}'s rolling token: {e:#}",
                self.session.as_str()
            ),
        }
    }

    /// Log a refusal once per (member, reason) pair. The trigger re-fires every
    /// tick, so announcing unconditionally would write one line per second for as
    /// long as the daemon's intent stands — but a refusal that says nothing at all
    /// leaves the session on its launch account invisibly.
    fn announce_refusal(&self, intended: &str, why: SwapRefused) {
        if self.should_announce(intended, &why) {
            logline!(
                "{}",
                refusal_line(self.session.as_str(), &self.member(), intended, &why)
            );
        }
    }

    /// The test seam: [`precondition_with`] with the models-equal rule.
    #[cfg(test)]
    fn precondition(&self, intended: &str) -> Result<SwapPlan, SwapRefused> {
        self.precondition_with(intended, ModelsRule::Equal)
    }

    /// The swap precondition under the models rule `models` names: a credit-cap
    /// leave relaxes it to [`ModelsRule::Any`] (see [`swap_eligible`]); every
    /// other caller passes [`ModelsRule::Equal`].
    ///
    /// Every arm refuses distinctly, so the log names the cause. The [`SwapPlan`]
    /// it returns is the touch step's only key: the install branch's
    /// `load_profile` (below, past the shared check) is what clears a legacy
    /// `.pending` credential sidecar, and moving the store's mtime before that
    /// clearing would discard the sidecar for good.
    ///
    /// A same-member call compares SOURCE identity before the `AlreadyCurrent`
    /// verdict: the one same-member shape admitted is the armed transition
    /// ([`converge_transition_holds`] — the canonical refreshable, the install
    /// source now selecting a distinct refreshless sidecar), which the
    /// in-place convergence executes. Every other same-member state stays a
    /// no-op.
    ///
    /// Every branch that installs a store re-reads the intended member's
    /// `stored_login` lock-free, like the drains: the converge branch after
    /// `install_source_path` + `converge_transition_holds` (it has no profile
    /// load), the install branch inside [`swap_target_precondition`] BEFORE the
    /// side-effect-free profile read. A store that parses with no login
    /// (`HoldsNone`) would install an empty login and sign the session out
    /// ([`SwapRefused::NoCredentialStore`]), and a store that exists but cannot
    /// be read (`Unknown` — a torn `credentials.json`, a torn sidecar, a key
    /// member's torn `config.toml`) is refused as
    /// [`SwapRefused::LoginUnreadable`] rather than linked on a guess, so no
    /// parse-error text reaches the line for those shapes.
    /// [`SwapRefused::ProfileUnreadable`] names a profile-read or
    /// `install_source_path` failure, plus the install branch's post-check
    /// `load_profile` (a tear between the check's read and the load). The
    /// converge branch fails `install_source_path` BEFORE its login re-read.
    fn precondition_with(
        &self,
        intended: &str,
        models: ModelsRule,
    ) -> Result<SwapPlan, SwapRefused> {
        let intended = ProfileName::from(intended);
        if self.shutdown.is_begun() {
            return Err(SwapRefused::ShuttingDown);
        }
        // The fixed background pair is the account's whole tree: every job on it
        // reads the one credential link, so a cross-account move would also be a
        // billing change under a running job. The same-member arm below is a
        // convergence, not a move, and still runs through `paths_for`.
        if self.tree.is_background() && intended != self.member() {
            return Err(SwapRefused::FixedAccount);
        }
        swap_support(self.mode).map_err(SwapRefused::Unsupported)?;
        // `--isolated` and fallback-following are mutually exclusive (a throwaway
        // tree versus swappable managed credentials). Enforced at the executor
        // because it is the one chokepoint every caller goes through, rather than
        // re-remembered by the decision leg and the flag separately.
        if self.isolation == Isolation::Isolated {
            return Err(SwapRefused::IsolatedSession);
        }
        if intended == self.member() {
            let store = crate::claude::install_source_path(&intended)
                .map_err(|e| SwapRefused::ProfileUnreadable(format!("{e:#}")))?;
            if !converge_transition_holds(&self.canonical(), &store) {
                return Err(SwapRefused::AlreadyCurrent);
            }
            match crate::profile::stored_login(&intended) {
                crate::profile::StoredLogin::HoldsLogin => {}
                crate::profile::StoredLogin::HoldsNone => {
                    return Err(SwapRefused::NoCredentialStore);
                }
                crate::profile::StoredLogin::Unknown => {
                    return Err(SwapRefused::LoginUnreadable);
                }
            }
            return Ok(SwapPlan {
                member: intended,
                store,
            });
        }
        let store = swap_target_precondition(&intended, &self.launch, models)?;
        // The shared check read the profile side-effect-free; the load below is
        // the executor's own cleanup — it clears a legacy `.pending` before the
        // swap's mtime stamp. It runs only past a passed check, so no refusal
        // the check would give is skipped; a tear between the check's read and
        // this load can still refuse `ProfileUnreadable`.
        crate::profile::load_profile(&intended)
            .map_err(|e| SwapRefused::ProfileUnreadable(format!("{e:#}")))?;
        Ok(SwapPlan {
            member: intended,
            store,
        })
    }

    /// Publish the swap into the cell: the member and canonical store the cell
    /// names become what the credential leg and teardown read, a freshly stamped
    /// marker joins `held`, and the refusal memo and any armed item retry clear.
    ///
    /// Called only after the link has moved, inside the same `with_state_lock`
    /// hold that spans the drain, the stamp and the repoint — the rank assert at
    /// the entry is that contract executable. A cell naming a member the link
    /// never reached would be permanent: `poll` filters on `member()` equality,
    /// so nothing retries, and the next tick would treat an interactive `/login`
    /// belonging to one member as the other's, writing it over a chain the
    /// session never authenticated as.
    ///
    /// Every member the session has run on keeps its marker for the session's
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
            // This swap's own legs own the item from here.
            cell.item_retry = None;
            cell.item_retry_stall = None;
        }
        debug_assert!(
            is_session_alive(&self.launch_marker),
            "the launch member's marker must still be held after publishing a swap. \
             Marker lifetime must not be shortened, or collect() probing current_member \
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
        self.swap_to_with(intended, ModelsRule::Equal)
    }

    /// [`swap_to`] with the models rule a credit-cap leave relaxes (see
    /// [`swap_eligible`]); every other caller keeps the models-equal rule.
    fn swap_to_with(&self, intended: &str, models: ModelsRule) -> Result<SwapOutcome> {
        let plan = match self.precondition_with(intended, models) {
            Ok(plan) => plan,
            Err(refused) => return Ok(SwapOutcome::Refused(refused)),
        };
        if plan.member.as_str() == self.member() {
            return self.converge_in_place(&plan);
        }
        let rotation = RotationGuard::acquire(&plan.member)?;
        // The repoint hands the session the incoming member's store (and on
        // macOS writes it into the session's item): a staged pair lands first.
        crate::oauth::land_stage_for_install(&plan.member, &rotation)
            .map_err(|refused| anyhow::anyhow!("{}", refused.text_with_status()))?;
        let link = self.runtime.join(".credentials.json");
        // The store whose login the session's Keychain item holds — what the
        // macOS carry-back below writes the item's pair into ([`item_carry_target`]).
        // Captured inside the hold, beside the publish that clears an armed
        // retry, and spent after it (the carry's read is a subprocess).
        #[cfg(target_os = "macos")]
        let mut previous_store: Option<std::path::PathBuf> = None;
        // The drain may copy a session-side `/login` of a DIFFERENT account into
        // the store: mark that account's `/profile` reading stale (no network
        // call) inside the same hold that committed the store.
        let outcome = with_state_lock(|_held| {
            let current = self.canonical();
            #[cfg(target_os = "macos")]
            {
                let armed = self
                    .cell()
                    .item_retry
                    .as_ref()
                    .map(|armed| armed.target.previous_store.clone());
                previous_store = Some(item_carry_target(armed.as_deref(), &current).to_path_buf());
            }
            // DRAIN. A Claude Code re-login sitting in the runtime file belongs to
            // the member the link STILL resolves to; once canonical moves, the
            // next tick would write those bytes into the new member's store and
            // its refresh token would be gone. A file the drain cannot judge
            // stays the session's: nothing below may repoint the link over it.
            match drain_session_login(&link, &current)? {
                Drain::Unchanged => {}
                Drain::LoginCopied => crate::usage::mark_profile_read_stale(&current),
                Drain::Unjudged => return Ok(SwapOutcome::Refused(drain_unjudged(&self.member()))),
            }

            let paths = self.paths_for(&plan.member)?;
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
            // Known gap: nothing is carried onto the incoming member's store
            // here (the global switch's `mcpOAuth` carry). The outgoing member
            // keeps the runtime file's non-login keys only where the drain above
            // saves them: a static-token member's sidecar through its sync, a
            // `credentials.json` member when the runtime file wins the drain.
            // On macOS the session's store is its per-session Keychain item, and
            // the swap's install replaces it without saving what sat beside the
            // outgoing member's login (`keychain::keychain_install_for_config_dir`).
            //
            // The incoming member joins the visit history BEFORE the link moves.
            // A launcher killed in the window between the two would otherwise
            // leave the tree reading an account its row names nowhere, idle to
            // delete, disable and rename. Recording first only over-approximates
            // — an account named but never linked stays live, the fail-closed
            // direction — and a row write that cannot land refuses the swap here,
            // while the tree is still on the outgoing member. `current_member`
            // keeps its later write point, with the row write that names the
            // link's new target.
            crate::live_sessions::update_as_session(self.session.as_str(), |fields| {
                fields.record_visited_member(plan.member.as_str());
            })?;
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
                    // The swap stamp lands here on EVERY platform, the instant
                    // the row first names the incoming member, so a 429 the
                    // outgoing account served just before this hold ages out
                    // instead of latching against the member it just moved to;
                    // macOS re-stamps it in `repoint_row_store` once a keychain
                    // leg lands, so a 429 served during the legs ages out
                    // against that later stamp. `set_launch_store` stays
                    // deferred on macOS: the session reads the item, not the
                    // file link, until a leg lands, so naming the incoming
                    // store here would exempt a rotation of the member it is
                    // still reading.
                    fields.set_last_swap_at(crate::usage::now_ms());
                    #[cfg(not(target_os = "macos"))]
                    fields.set_launch_store(plan.store.clone());
                })
            {
                logline!(
                    "clauth: session {} swapped onto {} but its row did not update: {e:#}",
                    self.session.as_str(),
                    plan.member
                );
            }
            Ok(SwapOutcome::Swapped)
        })?;
        // macOS: the session's Claude Code reads the NAMESPACED Keychain item
        // for this runtime dir (it sets `CLAUDE_CONFIG_DIR`, and CC resolves
        // the Keychain first), so the link repoint above moved only the file
        // layer. CARRY, then WRITE — or, when the incoming member's store is
        // refreshless, HAND OFF to the file (see [`swap_item_arm`]) — in that
        // order, and the second leg only runs if the carry could: the item
        // holds the outgoing member's only live pair
        // once CC has refreshed there (it deletes the runtime file on
        // migration), so writing before carrying would brick that member, and
        // skipping both on a failed carry leaves an inert swap — the session
        // keeps its previous credentials — rather than a destroyed chain. Both
        // legs run AFTER the flock (a `security` subprocess must never span
        // it); loud-not-fatal and idempotent. The executor refuses
        // `AlreadyCurrent`, so a failure that clears on its own (a locked
        // keychain) arms the cell's item retry for the swap poll to re-run
        // ([`SessionSwap::retry_item_leg_with`]); any other failure is
        // re-run only by a later swap onto ANOTHER member. The
        // carry's item read keeps its per-call `keychain::SECURITY_TIMEOUT`;
        // the write or sign-out gets one [`KEYCHAIN_MIRROR_BUDGET`] armed past
        // the carry's store write and the ownership row, so neither flock
        // wait spends it ([`owned_keychain_leg`]).
        //
        // The row's `launch_store` repoint rides the same ordering: until a leg
        // succeeds the session is still reading the outgoing member's pair out
        // of the item, so the row keeps naming that member's store and the
        // rotation verdict keeps refusing for it — the fail-closed direction.
        // No leg runs in a cfg(test) build (`keychain::enabled()` is false
        // there), so the file layer is the whole truth and the row follows the
        // link immediately, exactly as it does off macOS inside the hold.
        #[cfg(target_os = "macos")]
        if matches!(outcome, SwapOutcome::Swapped) && !crate::keychain::enabled() {
            self.repoint_row_store(&plan.member, &plan.store);
        }
        #[cfg(target_os = "macos")]
        if matches!(outcome, SwapOutcome::Swapped) && crate::keychain::enabled() {
            let target = previous_store.map(|previous_store| SwapItemRetry {
                member: plan.member.clone(),
                store: plan.store.clone(),
                previous_store,
            });
            let legs = match &target {
                Some(target) => self.run_swap_item_legs(target),
                None => Err(SwapItemFailure::Carry(anyhow::anyhow!(
                    "the outgoing member's install source is unknown"
                ))),
            };
            match legs {
                // The item holds the incoming member's pair, or is gone with
                // its MCP logins saved into the store this swap moved the file
                // layer onto: what the session reads moved, so the row's
                // verdict follows.
                Ok(()) => {
                    self.repoint_row_store(&plan.member, &plan.store);
                    self.retouch_after_item_leg(&plan.member, &plan.store);
                }
                Err(failure) => {
                    let (failed, rerun, retried) = failure.describe();
                    match (failure.retry_clears(), target) {
                        (Some(clears), Some(target)) => {
                            logline!(
                                "clauth: session {} swapped onto {} but {failed}; the watchdog \
                                 retries {retried} on this session's swap polls, backing off to \
                                 at most every {} s, while it stays on that member, {clears}",
                                self.session.as_str(),
                                plan.member,
                                ITEM_RETRY_MAX_GAP.as_secs()
                            );
                            self.arm_item_retry(target);
                        }
                        _ => logline!(
                            "clauth: session {} swapped onto {} but {failed}; only a later swap \
                             onto another member re-runs {rerun}",
                            self.session.as_str(),
                            plan.member
                        ),
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// The same-member convergence executor, reached only for a plan
    /// [`precondition`](Self::precondition) admitted: the canonical the cell
    /// names parses refreshable while the profile's install source now selects
    /// a distinct refreshless sidecar. Moves the session onto the sidecar
    /// without changing member — drain, clear the per-session Keychain item,
    /// then repoint the link.
    ///
    /// ONE rotation guard spans the sequence. The state flock is taken in two
    /// holds — the drain hold, then the commit hold after the keychain legs,
    /// because a `security` subprocess must never span the flock. The
    /// transition is REVALIDATED from disk at the head of the commit hold:
    /// a profile disarmed in the gap (or a sidecar re-filled with a rotating
    /// pair) stops the move with the row and cell untouched. Inside the
    /// commit hold the order is the swap's own: everything fallible runs
    /// before the link moves, and the cell publishes only once it has.
    ///
    /// macOS ordering is load-bearing: the item legs land BEFORE the commit.
    /// A failed carry or sign-out returns a refusal with nothing moved — the
    /// item still holds the rotating pair, so a repoint would strand the
    /// session on the pair while the row said sidecar, the fail-open
    /// direction. The bearer is never installed into the item.
    fn converge_in_place(&self, plan: &SwapPlan) -> Result<SwapOutcome> {
        let _rotation = RotationGuard::acquire(&plan.member)?;
        let link = self.runtime.join(".credentials.json");
        // The store being converged OFF of — what the macOS carry-back below
        // writes the session's Keychain pair into. Captured inside the hold.
        #[cfg(target_os = "macos")]
        let mut previous_store: Option<std::path::PathBuf> = None;
        // The drain may copy a session-side `/login` of a DIFFERENT account into
        // the store: mark that account's `/profile` reading stale (no network
        // call) inside the same hold that committed the store.
        let drained = with_state_lock(|_held| {
            let current = self.canonical();
            #[cfg(target_os = "macos")]
            {
                previous_store = Some(current.clone());
            }
            // DRAIN. A Claude Code re-login sitting in the runtime file belongs
            // to the member the link STILL resolves to; once canonical moves,
            // the next tick would write those bytes into the sidecar and the
            // refresh token would be gone. A file the drain cannot judge stays
            // the session's: the convergence stops with nothing moved.
            let drained = drain_session_login(&link, &current)?;
            if drained == Drain::LoginCopied {
                crate::usage::mark_profile_read_stale(&current);
            }
            Ok(drained)
        })?;
        if drained == Drain::Unjudged {
            return Ok(SwapOutcome::Refused(drain_unjudged(&self.member())));
        }
        // macOS: CARRY, then HAND OFF — in that order, and the hand-off only
        // runs if the carry could: the item may hold the freshest pair once CC
        // has refreshed there, and handing off over a failed carry would
        // destroy it. A failed leg returns a refusal with nothing moved — the
        // session stays on the rotating pair with the row and cell naming it,
        // and the next poll retries. A failure classified as the
        // locked-keychain transient (the carry's Err, the sign-out's
        // SkippedLocked) stops silently, and every other class announces once
        // per (member, class) through the cell's refusal memo — so neither
        // shape writes a line per tick while the keychain stays locked.
        // `enabled()` is a runtime check so a macOS `cfg(test)` build (no
        // keychain) converges on the file layer alone, exactly like the seed.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            let carried = previous_store
                .as_deref()
                .ok_or_else(|| {
                    anyhow::anyhow!("the rotating store this session converges off of is unknown")
                })
                .and_then(|store| crate::claude::carry_session_item_into(store, &self.runtime));
            match carried {
                Ok(()) => match converge_item_arm(
                    crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(
                        &plan.store,
                    )
                    .ok()
                    .as_ref(),
                ) {
                    SwapItemArm::SignOut => {
                        let sign_out = owned_keychain_leg(
                            KEYCHAIN_MIRROR_BUDGET,
                            || {
                                namespaced_keychain_ledger::authorize_write(
                                    &self.runtime,
                                    &plan.member,
                                    &self.session,
                                )
                            },
                            |owned| {
                                crate::keychain::keychain_hand_off_for_config_dir(
                                    &self.runtime,
                                    owned,
                                    &plan.store,
                                )
                            },
                        );
                        match sign_out {
                            Ok(crate::keychain::SignOutOutcome::SignedOut) => {}
                            // The item still serves the rotating pair (locked,
                            // or the sign-out failed): committing would repoint
                            // the link and row while the session keeps reading
                            // the item — the fail-open direction. Stop on the
                            // safe side; the next poll retries.
                            Ok(crate::keychain::SignOutOutcome::SkippedLocked) => {
                                return Ok(SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
                            }
                            Err(e) => {
                                let class = crate::keychain::classified_exit(&e);
                                // The classified locked-keychain transient is
                                // the steady state the next poll clears once
                                // the keychain unlocks; a line per tick for
                                // its whole duration is the noise the seed's
                                // disposition pattern exists to avoid. Silent
                                // — no memo, nothing to announce.
                                if converge_leg_disposition(class) == ConvergeLegDisposition::Silent
                                {
                                    return Ok(SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
                                }
                                if self.should_announce(
                                    plan.member.as_str(),
                                    &SwapRefused::ConvergeSignOutFailed(class),
                                ) {
                                    logline!(
                                        "clauth: session {} could not converge onto {}'s rolling \
                                         token: signing its per-session Keychain item out failed: \
                                         {e:#}. The session stays on the rotating pair; the next \
                                         poll retries",
                                        self.session.as_str(),
                                        plan.member
                                    );
                                }
                                return Ok(SwapOutcome::Refused(
                                    SwapRefused::ConvergeSignOutFailed(class),
                                ));
                            }
                        }
                    }
                    // Unreachable by admission (the transition selects a
                    // refreshless store) and never executed: installing the
                    // bearer would strand the session on a snapshot the
                    // re-stamp leg can no longer reach.
                    SwapItemArm::Install => {
                        return Ok(SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
                    }
                },
                Err(e) => {
                    let class = crate::keychain::classified_exit(&e);
                    // The classified locked-keychain transient is the steady
                    // state the next poll clears once the keychain unlocks; a
                    // line per tick for its whole duration is the noise the
                    // seed's disposition pattern exists to avoid. Silent — no
                    // memo, nothing to announce.
                    if converge_leg_disposition(class) == ConvergeLegDisposition::Silent {
                        return Ok(SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
                    }
                    if self.should_announce(
                        plan.member.as_str(),
                        &SwapRefused::ConvergeCarryFailed(class),
                    ) {
                        logline!(
                            "clauth: session {} could not converge onto {}'s rolling token: \
                             carrying the per-session Keychain pair back into the rotating store \
                             failed: {e:#}. The session stays on the rotating pair; the next poll \
                             retries",
                            self.session.as_str(),
                            plan.member
                        );
                    }
                    return Ok(SwapOutcome::Refused(SwapRefused::ConvergeCarryFailed(
                        class,
                    )));
                }
            }
        }
        with_state_lock(|_held| {
            // REVALIDATE the transition after the lock gap. Two holds cannot
            // pin it: `RotationGuard::acquire` blocks, and the keychain legs
            // above release the flock, so the profile could have been disarmed
            // (or the sidecar re-filled) since `precondition` read it. If it
            // no longer holds, stop on the safe side — nothing has moved, and
            // the row and cell stay truthful.
            if !converge_transition_holds(&self.canonical(), &plan.store) {
                return Ok(SwapOutcome::Refused(SwapRefused::AlreadyCurrent));
            }
            let current = self.canonical();
            let paths = self.paths_for(&plan.member)?;
            let claim = self.claim_markers(&paths)?;
            if matches!(claim, MarkerClaim::Foreign) {
                return Ok(SwapOutcome::Refused(SwapRefused::MarkerNotLockable));
            }
            // Re-checked in the hold for the same reason the swap's is: the
            // sidecar could have been removed since the revalidation above.
            if !plan.store.exists() {
                return Ok(SwapOutcome::Refused(SwapRefused::NoCredentialStore));
            }
            touch_store(plan, file_mtime(&current))?;
            relink_to_canonical(&link, &plan.store)?;

            // Past here the session IS on the sidecar, so nothing may report
            // otherwise. The row's `launch_store` follows in the same hold on
            // every platform: the item legs landed BEFORE this hold, so the
            // session's Claude Code already reads the file layer the link
            // just moved.
            self.publish_swap(plan, claim);
            if let Err(e) =
                crate::live_sessions::update_as_session(self.session.as_str(), |fields| {
                    fields.set_current_member(plan.member.as_str());
                    fields.set_last_swap_at(crate::usage::now_ms());
                    fields.set_launch_store(plan.store.clone());
                })
            {
                logline!(
                    "clauth: session {} converged onto {}'s rolling token but its row did not \
                     update: {e:#}",
                    self.session.as_str(),
                    plan.member
                );
            }
            Ok(SwapOutcome::Swapped)
        })
    }

    /// Point the row's rotation verdict at the store the swap repointed the
    /// link at, and RE-stamp the swap's `last_swap_at` — the hold already
    /// stamped it when the row first named the incoming member, and this moves
    /// it to the instant the session actually reads that member, which on macOS
    /// is only once a keychain leg lands (a 429 served during the legs must age
    /// out against this later stamp). Reached by the macOS item legs and the
    /// item retry, and always AFTER the state
    /// flock (each call site holds no lock, so `update_as_session` takes a
    /// FIRST-LEVEL acquisition there, not a reentry): the write loads a fresh row inside
    /// that hold, so it cannot revert a daemon-owned field, and a failed write
    /// leaves the row naming the outgoing member's store — rotations stay
    /// refused for the member the session may still be reading, never exempted
    /// for one it is not.
    fn repoint_row_store(&self, member: &ProfileName, store: &Path) {
        if let Err(e) = crate::live_sessions::update_as_session(self.session.as_str(), |fields| {
            fields.set_launch_store(store.to_path_buf());
            fields.set_last_swap_at(crate::usage::now_ms());
        }) {
            logline!(
                "clauth: session {} swapped onto {} but its row's store did not follow: {e:#}. \
                 Rotations stay refused for the member it left",
                self.session.as_str(),
                member
            );
        }
    }

    /// Move the store's mtime once more after the item leg landed.
    ///
    /// The swap stamps the store and repoints the link inside the hold, but the
    /// item leg runs after it. A request the session sent in between saw the
    /// stamped mtime, re-read the item while it still held the OUTGOING member,
    /// and memoized that mtime; Claude Code re-reads only when the mtime moves
    /// again, so the session would stay on the member the swap left. A second
    /// stamp makes every session that read in the gap re-read the item, which
    /// now holds the incoming login (or nothing, falling back to the file).
    /// Loud-not-fatal like the legs themselves: the swap already happened.
    /// Keyed on the member and store, not a [`SwapPlan`]: the swap's own
    /// plan-keyed [`touch_store`] already stamped this store, which is what
    /// [`stamp_store`] requires of a re-stamp.
    fn retouch_after_item_leg(&self, member: &ProfileName, store: &Path) {
        if let Err(e) = with_state_lock(|_held| stamp_store(member, store, file_mtime(store))) {
            logline!(
                "clauth: session {} swapped onto {} but re-stamping its store after the \
                 Keychain leg failed: {e:#}. A request sent during the swap may keep the \
                 session on its previous member until the store is next written",
                self.session.as_str(),
                member
            );
        }
    }

    /// The swap's per-session Keychain item legs, shared by the swap and its
    /// swap-poll retry: CARRY the item's pair back into the outgoing
    /// store, then INSTALL the incoming store into the item or, for a
    /// refreshless store, HAND the item OFF to the file. The arm is chosen
    /// from the store as it reads now, so a retry follows a store that turned
    /// refreshless since the swap. The caller holds the incoming member's
    /// rotation guard and no state flock.
    #[cfg(target_os = "macos")]
    fn run_swap_item_legs(&self, target: &SwapItemRetry) -> Result<(), SwapItemFailure> {
        crate::claude::carry_session_item_into(&target.previous_store, &self.runtime)
            .map_err(SwapItemFailure::Carry)?;
        // The refreshless skip the start site carries, at the one site where
        // the item already exists: a rolling sidecar or static mint swapped in
        // mid-session must never be INSTALLED into the item (CC would read that
        // snapshot forever and never return to the file the re-stamp leg
        // writes), and the outgoing member's login it still holds must go too,
        // or CC keeps serving the member the chain just moved the session off
        // of. The hand-off does both: it saves the item's MCP logins into the
        // incoming store and deletes the item, since an item left holding only
        // them reads as "Not logged in" and never reaches the file.
        match swap_item_arm(
            crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&target.store)
                .ok()
                .as_ref(),
        ) {
            SwapItemArm::Install => {
                let write = owned_keychain_leg(
                    KEYCHAIN_MIRROR_BUDGET,
                    || {
                        namespaced_keychain_ledger::authorize_write(
                            &self.runtime,
                            &target.member,
                            &self.session,
                        )
                    },
                    |owned| {
                        crate::claude::keychain_mirror_source_for_config_dir(
                            &target.store,
                            &self.runtime,
                            owned,
                        )
                    },
                );
                // Before the caller's row repoint: the row still names the
                // outgoing member's store, so the fan-out count adds this
                // session by construction rather than off its row. Only a
                // landed write created a new copy of the rotating pair, so the
                // failed outcome cannot raise the success-shaped warning.
                if let Some(line) =
                    fanout_warning(write.is_ok(), &target.member, &target.store, &self.session)
                {
                    logline!("{line}");
                }
                write.map_err(SwapItemFailure::Write)
            }
            SwapItemArm::SignOut => {
                let sign_out = owned_keychain_leg(
                    KEYCHAIN_MIRROR_BUDGET,
                    || {
                        namespaced_keychain_ledger::authorize_write(
                            &self.runtime,
                            &target.member,
                            &self.session,
                        )
                    },
                    |owned| {
                        crate::keychain::keychain_hand_off_for_config_dir(
                            &self.runtime,
                            owned,
                            &target.store,
                        )
                    },
                );
                match sign_out {
                    Ok(crate::keychain::SignOutOutcome::SignedOut) => Ok(()),
                    // The locked keychain left the item untouched, so the
                    // session still reads the outgoing member's login out of
                    // it: the row keeps naming that member's store, the same
                    // fail-closed direction a failed write takes.
                    Ok(crate::keychain::SignOutOutcome::SkippedLocked) => {
                        Err(SwapItemFailure::SkippedLocked)
                    }
                    Err(e) => Err(SwapItemFailure::SignOut(e)),
                }
            }
        }
    }

    /// The production leg [`retry_item_leg_with`](Self::retry_item_leg_with)
    /// runs: the swap's own item legs, their failure mapped onto the retry's
    /// outcome. A failure that still clears on its own stays silent (the
    /// swap's line already named the retry); any other announces once, as the
    /// retry drops.
    #[cfg(target_os = "macos")]
    fn swap_item_retry_leg(&self, target: &SwapItemRetry) -> SwapItemLeg {
        let failure = match self.run_swap_item_legs(target) {
            Ok(()) => return SwapItemLeg::Landed,
            Err(failure) => failure,
        };
        if failure.retry_clears().is_some() {
            return SwapItemLeg::Transient;
        }
        let (failed, rerun, _) = failure.describe();
        logline!(
            "clauth: session {} retried its swap onto {} but {failed}; only a later swap onto \
             another member re-runs {rerun}",
            self.session.as_str(),
            target.member
        );
        SwapItemLeg::Failed
    }

    /// Arm the swap poll's re-run of a swap's item legs.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "armed only by the macOS swap's item legs; the retry state machine is pinned on every platform"
        )
    )]
    fn arm_item_retry(&self, retry: SwapItemRetry) {
        // Arming resets the backoff: the first attempt runs on the next poll.
        self.cell().item_retry = Some(ArmedItemRetry {
            target: retry,
            due: Instant::now(),
            gap: Duration::ZERO,
        });
    }

    /// Put an attempted target back, due one backed-off gap after `now`.
    fn rearm_item_retry(&self, armed: ArmedItemRetry, now: Instant) {
        let gap = next_item_retry_gap(armed.gap);
        self.cell().item_retry = Some(ArmedItemRetry {
            target: armed.target,
            due: now + gap,
            gap,
        });
    }

    #[cfg(test)]
    fn item_retry(&self) -> Option<SwapItemRetry> {
        self.cell()
            .item_retry
            .as_ref()
            .map(|armed| armed.target.clone())
    }

    /// One swap poll's attempt at the item legs a swap left unlanded.
    ///
    /// A target the cell's member and store no longer match is dropped with
    /// no leg run: a later swap's own legs own the item. The legs write the
    /// incoming member's store into the item, so they run under that
    /// member's rotation guard, taken with `try_acquire`: a holder elsewhere
    /// skips this poll rather than parking the watchdog behind a rotation,
    /// and the target stays armed. A staged pair lands first, as the swap
    /// lands it. A lock that cannot be opened or a landing that cannot
    /// complete stays armed too, and says why once per (member, cause).
    /// `leg` runs outside every state-flock hold (a `security` subprocess
    /// must never span one).
    ///
    /// Only a landed leg moves the row's `launch_store` onto the incoming
    /// store, with the swap's re-stamp: until then the session reads the
    /// outgoing member's pair out of the item, so rotations stay refused for
    /// it.
    ///
    /// Attempts back off ([`next_item_retry_gap`]): the first runs on the
    /// first poll after arming, and every attempt that leaves the target
    /// armed (a transient, contention, a stall) pushes the next one out. A
    /// poll before it is due runs nothing and keeps the target armed.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            dead_code,
            reason = "driven only by the macOS swap poll; the retry state machine is pinned on every platform"
        )
    )]
    fn retry_item_leg_with(&self, leg: impl FnOnce(&SwapItemRetry) -> SwapItemLeg) {
        self.retry_item_leg_at(Instant::now(), leg);
    }

    /// [`retry_item_leg_with`](Self::retry_item_leg_with) at the instant
    /// `now`, so a test can step the backoff without waiting it out.
    fn retry_item_leg_at(&self, now: Instant, leg: impl FnOnce(&SwapItemRetry) -> SwapItemLeg) {
        let armed = {
            let mut cell = self.cell();
            let Some(armed) = cell.item_retry.take() else {
                return;
            };
            if cell.member != armed.target.member.as_str() || cell.canonical != armed.target.store {
                return;
            }
            if now < armed.due {
                cell.item_retry = Some(armed);
                return;
            }
            armed
        };
        let rotation = match RotationGuard::try_acquire(&armed.target.member) {
            Ok(Some(rotation)) => rotation,
            Ok(None) => {
                self.rearm_item_retry(armed, now);
                return;
            }
            Err(e) => {
                self.stall_item_retry(armed, now, format!("{e:#}"));
                return;
            }
        };
        if let Err(refused) = crate::oauth::land_stage_for_install(&armed.target.member, &rotation)
        {
            self.stall_item_retry(armed, now, refused.text_with_status());
            return;
        }
        match leg(&armed.target) {
            SwapItemLeg::Landed => {
                let target = &armed.target;
                self.repoint_row_store(&target.member, &target.store);
                self.retouch_after_item_leg(&target.member, &target.store);
                logline!(
                    "clauth: session {} finished its swap onto {}: its per-session Keychain leg \
                     landed on a swap-poll retry",
                    self.session.as_str(),
                    target.member
                );
            }
            SwapItemLeg::Transient => self.rearm_item_retry(armed, now),
            SwapItemLeg::Failed => {}
        }
    }

    /// Re-arm a retry that could not start, announcing `cause` only when it
    /// differs from the last one announced for that member: a later poll
    /// tries again, and a cause that persists is stated once.
    fn stall_item_retry(&self, armed: ArmedItemRetry, now: Instant, cause: String) {
        let member = armed.target.member.clone();
        self.rearm_item_retry(armed, now);
        let announced = {
            let mut cell = self.cell();
            let announced = cell
                .item_retry_stall
                .as_ref()
                .is_some_and(|(m, c)| *m == member && *c == cause);
            if !announced {
                cell.item_retry_stall = Some((member.clone(), cause.clone()));
            }
            announced
        };
        if !announced {
            logline!(
                "clauth: session {} could not retry its swap onto {}'s per-session Keychain leg: \
                 {cause}. The watchdog keeps retrying on its swap polls, backing off to at most \
                 every {} s",
                self.session.as_str(),
                member,
                ITEM_RETRY_MAX_GAP.as_secs()
            );
        }
    }

    /// Release and unlink every marker the swaps stamped. Called from `Drop`'s
    /// single teardown hold, and the mirror of the launch member's own leg: a
    /// marker dir goes only once no live marker is left in it.
    fn release_swapped_markers(&self) {
        // Taken out from under the cell first: the IO below acquires nothing, and
        // the rank is a true leaf only while it stays that way.
        let held = std::mem::take(&mut self.cell().held);
        for markers in held {
            let SwappedMarkers { pid_file, pid_lock } = markers;
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
            if let Some(dir) = pid_file.parent()
                && prune_stale_sessions(dir).unwrap_or(1) == 0
            {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

/// One session's three reconcile legs, as one value the watchdog loop calls
/// back into.
struct WatchdogLegs {
    claude_home: PathBuf,
    /// Also holds, on macOS, the swap's item retry target while a transient
    /// left a swap's item legs unlanded (see
    /// [`SessionSwap::retry_item_leg_with`]); a swap poll re-runs it before
    /// polling on a due attempt.
    swap: std::sync::Arc<SessionSwap>,
    /// macOS: the seed's retry target while a classified locked keychain left
    /// the session's item unwritten — see `retry_seeded_keychain_item`. The
    /// credential tick recomputes the seed against it: no timer of its own, one
    /// bounded subprocess budget per attempt, and it drops the moment the store
    /// stops matching the target or a retry fails on anything other than the
    /// classified transient.
    #[cfg(target_os = "macos")]
    seed_retry: std::sync::Mutex<Option<PathBuf>>,
}

impl crate::watchdog::Reconcile for WatchdogLegs {
    fn config(&self) {
        // Try-once: a peer mid-merge must not park the credential leg and swap
        // poll on the same loop iteration. A skipped pass retries on the next
        // event or the 30 s fallback tick.
        if let Err(e) = crate::claude_json::sync_once_try() {
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
        // After the file reconcile and outside every lock: the retry shells
        // out, so it must never span the state flock `tick` takes and releases
        // itself.
        #[cfg(target_os = "macos")]
        retry_seeded_keychain_item(&self.swap, &self.seed_retry);
    }
    fn swap_poll(&self) {
        // Before the poll, outside every lock: a swap's unlanded item legs
        // land on a due attempt once the keychain unlocks (the backoff spaces
        // attempts while it stays locked). Until they land the poll's
        // convergence waits, and a fresh swap carries the item into the
        // retry's outgoing store, so the outgoing member's login never reaches
        // another member's store.
        #[cfg(target_os = "macos")]
        self.swap
            .retry_item_leg_with(|target| self.swap.swap_item_retry_leg(target));
        self.swap.poll();
    }
    #[cfg(test)]
    fn tick_driven(&self) {
        self.swap
            .tick_reconciles
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Arm this session's fs watcher and spawn its watchdog thread, named for the
/// account the session runs on. The watcher is armed HERE rather than on the
/// spawned thread, so that the acquiring call returning IS the barrier proving
/// the watch is live (arming costs 18-34 ms on macOS; a credential write
/// landing in the window a spawn-first caller could not see would wait out the
/// whole 30 s fallback). The returned sender signals the watchdog to exit when
/// dropped (see [`ProfileRuntime::drop`]).
#[expect(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
fn spawn_session_watchdog(
    claude_home: PathBuf,
    swap: std::sync::Arc<SessionSwap>,
    name: &str,
    #[cfg(target_os = "macos")] seed_retry: Option<PathBuf>,
) -> Result<(crossbeam_channel::Sender<()>, JoinHandle<()>)> {
    let (watchdog_tx, watchdog_rx) = crossbeam_channel::bounded::<()>(1);
    let legs = WatchdogLegs {
        claude_home: claude_home.clone(),
        swap: std::sync::Arc::clone(&swap),
        #[cfg(target_os = "macos")]
        seed_retry: std::sync::Mutex::new(seed_retry),
    };
    let specs = crate::watchdog::watch_specs(
        swap.runtime.as_path(),
        swap.canonical().as_path(),
        &claude_home,
    );
    let requested = specs.len();
    let watcher = crate::watchdog::try_start(&specs, crate::watchdog::PRODUCTION.debounce);
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
    Ok((watchdog_tx, watchdog_handle))
}

/// Live-session guard. On drop: stops the watchdog, runs a final sync
/// (errors surface to stderr), drops the PID file, and retires this session's
/// own runtime tree.
pub(crate) struct ProfileRuntime {
    /// Shared with the watchdog thread: the member the credential link resolves
    /// to, which a swap moves, plus everything that swap needs.
    swap: std::sync::Arc<SessionSwap>,
    pid_file: PathBuf,
    claude_home: PathBuf,
    sessions: PathBuf,
    /// Notes raised while building the runtime dir — one per shared store the
    /// build could not create — held so the caller surfaces them once. The
    /// foreground start prints them; the MCP delegate path stays silent.
    store_notes: Vec<String>,
    /// Held for the lifetime of the session so a sibling process's
    /// `try_lock` reveals we're still alive.
    _pid_lock: File,
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
/// leaving the acquire to re-create the profile dir, build a tree, stamp its
/// marker and register a live row for an account nothing configures.
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
/// WAITS behind a rotation, and a rotation is precisely what leaves a stage
/// (`credentials.json.staged`, or a pre-fix binary's legacy `.pending`) for the
/// next load to pick up. The wait's
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
        launcher: crate::live_sessions::Launcher,
    ) -> Result<Self> {
        Self::acquire_synced(
            &profile.name,
            isolation,
            stale_env_keys,
            follows_chain,
            launcher,
            || {},
            |_, _| {},
            || {},
        )
    }

    /// Two injected sync points, both no-ops in production — the same shape
    /// `claude::arm_rolling_from_disk_synced` already uses.
    ///
    /// `pre_lock_done` runs after the rotation guard and immediately before the
    /// state flock, for the regression tests that pose the window there — a
    /// record removal, and a profile-field edit — so "the gate reads the record
    /// from INSIDE the hold" and "the tree is fed the re-read copy" are each
    /// pinned by construction rather than by a thread race nothing can
    /// schedule.
    ///
    /// Each poses ONE window — a record removal, or a profile-field edit —
    /// between the rotation guard and the flock acquisition. What shuts each
    /// window differs, and the difference is why the edit test exists at all:
    /// a same-version REMOVAL cannot be posed from a second thread, because
    /// delete/rename `try_acquire` the rotation guard and give up rather than
    /// queue (see `refuse_if_unconfigured`), while a field edit persists
    /// through `save_profile` under the state flock alone and never touches
    /// the rotation guard — so a concurrent edit is ordinary, not exotic, and
    /// the thing that keeps it off a session is the in-hold re-read below,
    /// serialized against that same flock. Neither seam can pose the narrower
    /// window on the other side of the flock acquisition — that one wants a
    /// thread holding the flock — so placement is pinned by `debug_assert!`s
    /// instead: `refuse_if_unconfigured`'s for the gate, the re-read's own
    /// for the field copy.
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
    /// are each satisfied by a sibling session's marker or row, so a probe written
    /// against them passes with this session's own marker unclaimed.
    ///
    /// `hold_released` runs after the drop and asks the one thing the other cannot:
    /// that the lock is free by then. It needs no fixed position now that the
    /// artifacts are pinned inside the closure — restoring the long hold makes it
    /// see the lock HELD wherever it sits.
    #[allow(clippy::too_many_arguments)]
    fn acquire_synced(
        name: &ProfileName,
        isolation: Isolation,
        stale_env_keys: &[String],
        follows_chain: bool,
        launcher: crate::live_sessions::Launcher,
        pre_lock_done: impl FnOnce(),
        stamp_window_closing: impl FnOnce(&SessionPaths, &SessionId),
        hold_released: impl FnOnce(),
    ) -> Result<Self> {
        Self::acquire_tree(
            name,
            isolation,
            stale_env_keys,
            follows_chain,
            launcher,
            SessionTree::PerSession,
            None,
            pre_lock_done,
            stamp_window_closing,
            hold_released,
        )
    }

    /// Enter the account's fixed background pair as this process's own holder:
    /// the one codepath that serves both a `clauth start <p> -- --bg` launcher
    /// and the daemon's adoption, so a second holder meets the same additive
    /// build and the same "no live marker AND no job names the tree" wipe rule
    /// a first one does.
    ///
    /// `hold` decides only how the registry row marks the hold: the daemon's
    /// own hold is not a "live session" for the macOS rotation gate's marker
    /// term, while a launcher's is. `flag` is the launcher's background flag,
    /// echoed as typed into the copy-host refusal; the daemon passes `None`.
    pub(crate) fn acquire_background(
        name: &ProfileName,
        hold: BackgroundHold,
        stale_env_keys: &[String],
        flag: Option<&str>,
    ) -> Result<Self> {
        Self::acquire_tree(
            name,
            Isolation::Shared,
            stale_env_keys,
            false,
            crate::live_sessions::Launcher::Own,
            SessionTree::Background(hold),
            flag,
            || {},
            |_, _| {},
            || {},
        )
    }

    /// Adopt a handed-off `--with-fallback` session into this process's hold:
    /// the daemon's own [`ProfileRuntime`] for the per-launch tree a Claude
    /// Code job still names, after the launcher exited and handed the row on.
    ///
    /// The row is the hand-off record: the start profile, the member the tree
    /// last ran on, the boot transport, and every member the session has run
    /// on all live there, and the adoption keeps them as they are — only the
    /// row's pid and hold mark change. The markers it claims are the current
    /// member's own `sessions-<sid>/<sid>` (the launch member's when the
    /// session never moved) plus every earlier member's, all claimed before the
    /// row is written, and the sid is never re-minted: a live marker or a stop
    /// in flight refuses the adoption for a later retry. The job's own
    /// supervisor lives beside the hold for the job's whole life, so the
    /// adoption never waits on it.
    pub(crate) fn acquire_adopted(sid: &str, _stale_env_keys: &[String]) -> Result<Self> {
        if !is_session_id(sid) {
            anyhow::bail!("not a session id: {sid:?}");
        }
        let session = SessionId::from_validated(sid.to_string());
        let claude_home = claude_dir()?;
        if !claude_home.exists() {
            anyhow::bail!("~/.claude not found; install Claude Code first");
        }

        // The member the tree runs on decides which rotation lock guards the
        // adoption and which marker the hold claims. Read once here to pick it;
        // the authoritative row is re-read inside the state flock below.
        let member_name = crate::live_sessions::get(sid)
            .map(|row| {
                ProfileName::from(row.current_member.as_deref().unwrap_or(&row.start_profile))
            })
            .ok_or_else(|| anyhow::anyhow!("no live-session row"))?;
        // Test seam: the session moves accounts in exactly this window (after
        // the pre-lock member read, before the in-flock re-read), which the
        // re-read below must catch. Nothing outside tests sets this.
        #[cfg(test)]
        run_adopt_reread_hook();
        let rotation_guard =
            RotationGuard::acquire_with_timeout(&member_name, rotation_lock_timeout())?;
        // A live stage a failed persist left behind lands first, before the
        // rules read or write the store, or they judge against a store that
        // lags the chain (the same lander `acquire_tree` runs).
        crate::oauth::land_stage_for_install(&member_name, &rotation_guard)
            .map_err(|refused| anyhow::anyhow!("{}", refused.text_with_status()))?;

        // The item legs the adopted row records as unlanded, decided inside the
        // hold that reads the row. Off macOS the row's store follows the link
        // inside the swap's own hold, so it never records one.
        #[cfg(target_os = "macos")]
        let mut item_retry: Option<SwapItemRetry> = None;
        let (start_paths, member_paths, pid_lock, mode, member, canonical, launch, held) =
            with_state_lock(|flock| {
                let row = crate::live_sessions::get(sid)
                    .ok_or_else(|| anyhow::anyhow!("no live-session row"))?;
                if !row.follows_chain || row.isolated {
                    anyhow::bail!(
                        "this session did not opt into the fallback chain; refusing to adopt it"
                    );
                }
                let start_profile = ProfileName::from(row.start_profile.as_str());
                let member =
                    ProfileName::from(row.current_member.as_deref().unwrap_or(&row.start_profile));
                // The rotation guard above was taken for the member read BEFORE
                // the flock. A predecessor hold finishing a swap during this
                // wait moves the session, so the guard would cover the wrong
                // account: bail for a retry rather than adopt under it.
                if member != member_name {
                    anyhow::bail!("the session moved accounts while the adoption waited; retrying");
                }
                // The tree is the start profile's per-session runtime dir; the
                // marker is the member's. A copy host could not have launched
                // this session (the launch refuses), but the mode is probed
                // here so a flip refuses through the same typed path the fixed
                // pair uses, never as an ordinary adoption failure.
                let profile_root = profile_dir(&start_profile)?;
                crate::profile::mkdir_700(&profile_root)
                    .with_context(|| format!("failed to create {}", profile_root.display()))?;
                let mode = detect_link_mode(&profile_root)?;
                if mode == LinkMode::Fake {
                    return Err(anyhow::Error::new(CopyHostBackgroundRefusal));
                }
                let start_paths = resolve_tree(
                    &start_profile,
                    Isolation::Shared,
                    SessionTree::PerSession,
                    &session,
                    mode,
                )?;
                let member_paths = resolve_tree(
                    &member,
                    Isolation::Shared,
                    SessionTree::PerSession,
                    &session,
                    mode,
                )?;
                // A GC/teardown is between its decide and removal holds for the
                // tree: adopting now would hold a tree the sweep is about to
                // end. The next retry adopts once it settles.
                if stop_in_flight(&start_paths.runtime)? {
                    anyhow::bail!(
                        "clauth is stopping Claude Code's background service in {}; \
                         retry in a moment",
                        start_paths.runtime.display()
                    );
                }
                // Markers are claimed before the row write and before the tree
                // is touched, past the cheap refusals above, so a refused
                // adoption never rewrites a live session's tree. The live
                // Claude Code child keeps every refresh token it was ever
                // handed, so the hold claims the marker of every member the
                // session has run on, not only the one it runs on now: the
                // visitors first, then the current member, whose marker is the
                // hold's own pid file. Each other member's marker joins the
                // swap cell's `held`, released at teardown like a live swap's.
                // A member whose profile dir is gone is skipped and logged,
                // never fatal. A foreign holder on ANY marker refuses the
                // adoption for a retry, and `claims` then takes back the marker
                // files and `sessions-<sid>/` dirs this attempt made — a
                // holder's own file and a dir that stood before the attempt are
                // left alone, so the refusal leaves no file of its own.
                let mut claims = AdoptionClaims::default();
                let mut held: Vec<SwappedMarkers> = Vec::new();
                let mut history = row_live_members(&row);
                history.retain(|visited| visited != member.as_str());
                history.sort();
                history.dedup();
                for visited in history {
                    let visited_name = ProfileName::from(visited.as_str());
                    let Ok(visited_dir) = profile_dir(&visited_name) else {
                        logline!(
                            "clauth: skipping marker for {visited}, whose profile dir \
                             cannot be resolved"
                        );
                        continue;
                    };
                    if !visited_dir.is_dir() {
                        logline!(
                            "clauth: skipping marker for {visited}, whose profile dir is gone"
                        );
                        continue;
                    }
                    let visited_paths = resolve_tree(
                        &visited_name,
                        Isolation::Shared,
                        SessionTree::PerSession,
                        &session,
                        mode,
                    )?;
                    claims.begin(&visited_paths);
                    match stamp_swapped_markers(&visited_paths)? {
                        Some(markers) => held.push(markers),
                        None => {
                            anyhow::bail!(
                                "failed to claim session marker {}: another live process \
                                 holds this session id",
                                visited_paths.pid_file.display()
                            );
                        }
                    }
                }
                claims.begin(&member_paths);
                let Some(current) = stamp_swapped_markers(&member_paths)? else {
                    anyhow::bail!(
                        "failed to claim session marker {}: another live process \
                         holds this session id",
                        member_paths.pid_file.display()
                    );
                };
                let pid_lock = current.pid_lock;
                // The launch read is hoisted above the drain: `load_profile`
                // can refuse, and a refusal must come before the first store
                // write or relink.
                let launch = match row.boot_transport.clone() {
                    Some(transport) => transport,
                    None => LaunchTransport::of(&crate::profile::load_profile(&start_profile)?),
                };
                // Claude Code saves its login over the tree's credential link
                // as a regular file between the launcher's exit and this
                // adoption (or between holds after a daemon outage): `claude
                // --bg` 2.1.310 re-saves it with one more key about a second
                // after launch, and a token refresh lands the same way. The
                // drain runs only after every marker is claimed, so a refused
                // adoption never rewrites a tree or store it could not hold.
                let link_path = start_paths.runtime.join(".credentials.json");
                if !link_path
                    .symlink_metadata()
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    adopt_saved_login(&link_path, &member, flock)?;
                }
                // The tree is the launcher's own: its credential link must
                // resolve to one of the member's own stores — the
                // `credentials.json` or the `session-token.json` sidecar — so
                // the adoption never guesses an account the row does not name.
                // The swap cell seeds the LINK TARGET, not today's install
                // source: the convergence leg then moves a link still on the
                // other store onto the current install source.
                let link = crate::canonical::identity(&link_path).ok();
                let member_dir = profile_dir(&member)?;
                let own_stores = [
                    crate::canonical::identity(member_dir.join("credentials.json")).ok(),
                    crate::canonical::identity(member_dir.join("session-token.json")).ok(),
                ];
                let canonical = link
                    .filter(|link| own_stores.iter().flatten().any(|store| store == link))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "the session's credential link does not resolve to {member}'s \
                             own stores; refusing to adopt it"
                        )
                    })?;
                claims.commit();
                // Re-register the row under this holder: the handed-off fields
                // stay; only the pid and the hold mark change.
                let mut adopted_row = row;
                adopted_row.pid = std::process::id();
                adopted_row.background_hold = true;
                if let Err(e) = crate::live_sessions::register(&adopted_row) {
                    logline!("clauth: registering the live session failed: {e}");
                }
                // In the link's identity form, so a spelling of the same store
                // never reads as an unlanded leg.
                #[cfg(target_os = "macos")]
                {
                    let launch_store = adopted_row
                        .launch_store
                        .map(|store| crate::canonical::identity(&store).unwrap_or(store));
                    item_retry = adopted_item_retry(launch_store.as_deref(), &canonical, &member);
                }
                Ok::<_, anyhow::Error>((
                    start_paths,
                    member_paths,
                    pid_lock,
                    mode,
                    member,
                    canonical,
                    launch,
                    held,
                ))
            })?;
        drop(rotation_guard);

        let swap = std::sync::Arc::new(SessionSwap::from_parts(
            session,
            Isolation::Shared,
            mode,
            SessionTree::PerSession,
            launch,
            member.as_str().to_string(),
            canonical,
            start_paths.runtime.clone(),
            member_paths.pid_file.clone(),
            held,
        ));
        #[cfg(target_os = "macos")]
        if let Some(retry) = item_retry {
            swap.arm_item_retry(retry);
        }
        let (watchdog_signal, watchdog_handle) = spawn_session_watchdog(
            claude_home.clone(),
            std::sync::Arc::clone(&swap),
            member.as_str(),
            #[cfg(target_os = "macos")]
            None,
        )?;
        Ok(Self {
            swap,
            pid_file: member_paths.pid_file,
            claude_home,
            sessions: member_paths.sessions,
            store_notes: Vec::new(),
            _pid_lock: pid_lock,
            watchdog_signal: Some(watchdog_signal),
            watchdog_handle: Some(watchdog_handle),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn acquire_tree(
        name: &ProfileName,
        isolation: Isolation,
        stale_env_keys: &[String],
        follows_chain: bool,
        launcher: crate::live_sessions::Launcher,
        tree: SessionTree,
        flag: Option<&str>,
        pre_lock_done: impl FnOnce(),
        stamp_window_closing: impl FnOnce(&SessionPaths, &SessionId),
        hold_released: impl FnOnce(),
    ) -> Result<Self> {
        // The background pair is the shared flavour on both link modes, so the
        // tree build, the settings merge and the credential link all take the
        // shared arm whatever the caller asked for.
        let isolation = if tree.is_background() {
            Isolation::Shared
        } else {
            isolation
        };
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
        // The tree below links or copies the STORE, and on macOS the seed writes
        // it into the session's Keychain item: a pair a failed persist left
        // staged lands first, before this start's own state-flock hold, or the
        // start is refused rather than seeded from a store that lags the chain.
        crate::oauth::land_stage_for_install(name, &rotation_guard)
            .map_err(|refused| anyhow::anyhow!("{}", refused.text_with_status()))?;
        pre_lock_done();

        let (session, paths, pid_lock, mode, fresh, notes, dead) = with_state_lock(|_held| {
            // Inside the hold, and ahead of every write this closure does — see
            // `refuse_if_unconfigured` for which mechanism each half buys.
            refuse_if_unconfigured(name)?;
            // The record passed the gate; now re-read its FIELDS from disk,
            // inside the same hold: `acquire_synced` carries only the name, so
            // this is the one copy a base_url/api_key/env/models edit cannot
            // slip past — the caller's profile predates the rotation-guard
            // wait above, and an edit landing in that window must feed this
            // session's tree from here.
            //
            // `load_profile` can WRITE under this hold: it adopts a legacy
            // `credentials.json.pending` sidecar a pre-fix rotation left, and
            // rewrites a semantically-drifted config.toml. Both are safe here —
            // each re-enters the state flock on the designed reentrant path
            // (`live_sessions::register` takes the same one below), and neither
            // fires on an ordinary start, only on a crashed rotation's residue
            // or a hand-edited config.
            debug_assert!(
                crate::lockorder::holds::<crate::lockorder::rank::State>(),
                "the profile-field re-read must happen under the state flock, \
                 or a field edit serialized on that flock can land between the \
                 read and the tree it feeds"
            );
            let fresh = crate::profile::load_profile(name)?;
            // The transport is probed FIRST: it picks link vs copy for the build
            // and, for a SHARED session, whether the tree is the bare stem under
            // `LinkMode::Fake` or keyed per session. The mode must be known
            // before every path below.
            // The profile dir is the probe site because it exists independently
            // of the tree — created here rather than assumed, so nothing rests on
            // `RotationGuard::acquire` having made it.
            crate::profile::mkdir_700(&profile_root)
                .with_context(|| format!("failed to create {}", profile_root.display()))?;
            let mode = detect_link_mode(&profile_root)?;
            // The floor under `start::admit`'s copy-host refusal: a `--bg`
            // launcher is refused there before anything spawns, but the daemon
            // reaches `acquire_background` directly, so it must refuse here too
            // — under the state lock, on the SAME mode the build below consumes
            // — or a copy host would claim a marker and build a fixed pair a
            // background session would outlive (the copied tree cannot follow a
            // rotation that relinks the credentials it holds). A probe that
            // failed already propagated as the `?` above, so a copy host can
            // never read as linkable (fail closed). The launcher arm reuses the
            // approved copy-host refusal; the daemon arm is a TYPED refusal the
            // daemon re-queues on its own schedule (once per job-set spell,
            // never a backoff retry), not an adoption failure to retry.
            if mode == LinkMode::Fake
                && let SessionTree::Background(hold) = tree
            {
                match hold {
                    BackgroundHold::Launcher => {
                        let flag = flag.unwrap_or("--bg");
                        anyhow::bail!("{}", copy_host_background_refusal(name.as_str(), flag));
                    }
                    BackgroundHold::Daemon => {
                        return Err(anyhow::Error::new(CopyHostBackgroundRefusal));
                    }
                }
            }
            // A sid is a NAME, not a claim. `<pid>-<seq>` collides only when a
            // second LIVE process minted the same pair, which needs a `~/.clauth`
            // shared across pid namespaces, or an NFS home, and the collision
            // lands on this session's OWN marker. Re-mint rather than wait: the
            // claim below runs inside the state flock, so a blocking
            // wait there wedges every other clauth process on this home, and
            // `is_session_alive` reads every unknown as live, so an unreadable
            // marker moves this session aside instead of parking it.
            let mut session = SessionId::mint();
            let mut paths = resolve_tree(name, isolation, tree, &session, mode)?;
            // One bounded mint loop covers every reason to move off a path: the
            // marker is claimed (`SID_COLLISION_REMINTS`' original shape), a
            // supervisor stop is in flight for it, or it holds a live
            // supervisor the wipe below must not delete. The Fake shared tree
            // and the background pair keep one runtime path whatever the sid,
            // so there the stop refuses and the wipe declines.
            let fixed_runtime = matches!(tree, SessionTree::Background(_))
                || (mode == LinkMode::Fake && isolation == Isolation::Shared);
            for _ in 0..SID_COLLISION_REMINTS {
                // A GC/teardown is between its decide and removal holds for this
                // path; launching a session whose shared supervisor it is about
                // to end would be stopped mid-flight.
                if stop_in_flight(&paths.runtime)? {
                    if fixed_runtime {
                        anyhow::bail!(
                            "clauth is stopping Claude Code's background service in {}; \
                             retry in a moment",
                            paths.runtime.display()
                        );
                    }
                    session = SessionId::mint();
                    paths = resolve_tree(name, isolation, tree, &session, mode)?;
                    continue;
                }
                if is_session_alive(&paths.pid_file) {
                    session = SessionId::mint();
                    paths = resolve_tree(name, isolation, tree, &session, mode)?;
                    continue;
                }
                // A registry row outlives its pid once its session is handed
                // off: a row already naming this id is the hand-off record,
                // which a fresh acquire must never overwrite. Re-mint rather
                // than wait, like the marker collision.
                if crate::live_sessions::get(session.as_str()).is_some() {
                    session = SessionId::mint();
                    paths = resolve_tree(name, isolation, tree, &session, mode)?;
                    continue;
                }
                // A live supervisor at this path must never be wiped; move off
                // it (a fixed runtime path keeps its supervisor and the wipe
                // below is declined).
                if !fixed_runtime && supervisor_may_be_alive(&paths.runtime) {
                    session = SessionId::mint();
                    paths = resolve_tree(name, isolation, tree, &session, mode)?;
                    continue;
                }
                break;
            }
            let SessionPaths {
                runtime,
                sessions,
                pid_file,
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
            // The tree is cleared out of the path so stale symlinks/copies to
            // entries that have since vanished from ~/.claude/ don't carry over
            // into the build. A live sibling holds a marker here, so its tree is
            // never the one cleared. A live `claude --bg` job names its tree here
            // too (the marker flock never sees the session), so a tree the job
            // store names — or one an unreadable store cannot clear — is skipped
            // as well; the store read runs only when a stale tree sits at the
            // path. A tree whose own supervisor may recreate it is never cleared
            // here (no subprocess, no signal inside the flock) — the mint loop
            // above moved sid-keyed layouts onto a fresh path, and a fixed
            // runtime path keeps its supervisor via the additive build below.
            //
            // A dead tree can hold the only copy of its session's transcripts
            // and sidecars (a recycled `<pid>-<seq>` lands on one no sweep has
            // rescued yet), so it is renamed to its rescue tombstone — O(1), the
            // one step this hold pays — and rescued once the hold drops, as GC
            // does ([`retire_tree`]). A failed rename (a kept tombstone already
            // holds the name, an open handle on Windows) keeps the tree and
            // builds over it, never a wipe: the build is additive, and the
            // tree's next teardown or sweep rescues it. A dead runtime path that
            // is itself a link is unlinked; a link the checks above spare, or
            // one that would not unlink, refuses the start, since the build
            // would write through it.
            let mut tombstone = None;
            if active == 0
                && runtime.symlink_metadata().is_ok()
                && !crate::claude_jobs::job_trees().spares(runtime)
                && !supervisor_may_be_alive(runtime)
            {
                tombstone = retire_tree(runtime);
            }
            if crate::sessions::is_symlink(runtime) {
                anyhow::bail!(
                    "{} is a link, not a runtime tree clauth made; remove it and start again",
                    runtime.display()
                );
            }
            crate::profile::mkdir_700(runtime)
                .with_context(|| format!("failed to create {}", runtime.display()))?;
            let store_notes = build_runtime_dir_with_active_env(
                runtime,
                &claude_home,
                &fresh,
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

            // Register inside this same hold, once the marker is flock-held: the
            // row can then never exist without a liveness signal for GC to test
            // it by, and `register`'s own `with_state_lock` takes the reentrant
            // path instead of a second 25s-bounded flock acquisition. A registry
            // failure is reported and stepped over — the session itself is
            // already sound, and failing here would trade a missing row for a
            // dead session.
            let opt_in = chain_opt_in_survives(follows_chain, isolation, mode);
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
            let mut row = crate::live_sessions::LiveSession::starting(
                &session,
                name,
                crate::harness::Harness::Claude,
                isolation == Isolation::Isolated,
                opt_in,
                launcher,
                // The SAME value the runtime tree is built from below, so the
                // row cannot disagree with what this session actually reads —
                // on macOS, which is the only place it is consulted. Every
                // later swap repoints it at the member the session lands on
                // (`swap_to`'s row update; on macOS that waits for the
                // keychain legs), so `live_session_holds_rotatable` reads the
                // store of the member the session is ON, never the launch one.
                //
                // The daemon's background hold leaves it `None`: that hold is
                // exempt from the rotation gate's marker term, and a store path
                // would otherwise make it read as a live session sharing the
                // account's rotating login in `sessions_holding_store`.
                match tree {
                    SessionTree::Background(BackgroundHold::Daemon) => None,
                    _ => Some(canonical.clone()),
                },
                // The boot transport the executor snapshots from `fresh` below:
                // the row's record is the SAME value, so the decision leg judges
                // members against what the child actually runs.
                Some(crate::runtime::LaunchTransport::of(&fresh)),
            );
            if let SessionTree::Background(hold) = tree {
                row.background = true;
                row.background_hold = hold == BackgroundHold::Daemon;
            }
            if let Err(e) = crate::live_sessions::register(&row) {
                logline!("clauth: registering the live session failed: {e}");
            }
            stamp_window_closing(&paths, &session);
            Ok::<_, anyhow::Error>((session, paths, file, mode, fresh, store_notes, tombstone))
        })?;
        // The `.claude.json` seed records its write under `JsonSyncLock`, whose
        // rank (480) sits OUTSIDE `State` (500): it cannot run inside the
        // state-flock hold above without a rank inversion. Seeding here — past
        // the flock, still before the session launches — keeps the copy's
        // carried rank recorded.
        // A failed seed fails the start, so a started row is never left behind
        // by a seed that could not write a copy: the row and the marker this
        // start registered are released before the error propagates.
        if let Err(e) = seed_claude_json(&paths.runtime, &claude_home) {
            let _ = crate::live_sessions::unregister(session.as_str());
            let _ = std::fs::remove_file(&paths.pid_file);
            return Err(e);
        }
        // macOS: the Keychain half of the tree build. Before the guard drops,
        // so a rotation queued behind it cannot land between the build and the
        // item write — see `seed_session_keychain_item`.
        #[cfg(target_os = "macos")]
        let seed_retry = seed_session_keychain_item(&canonical, &paths.runtime, name, &session);
        // Released at the end of the register-and-stamp window rather than at the
        // end of this function, so a queued peer waits out that window and not the
        // watchdog arming behind it. On macOS the guard additionally spans
        // `seed_session_keychain_item`'s `security` subprocesses, which sit past
        // the state flock on purpose (a subprocess must never span it).
        //
        // What the hold must still cover, and does: the credential
        // materialization inside `build_runtime_dir_with_active_env` (which
        // samples the chain — a byte copy under `LinkMode::Fake`, a relink plus a
        // possible adopt under `Real`), the marker `try_lock`, and the registry
        // row. Those last two are the whole of what the rotation gate's marker
        // term reads — `has_live_marker` walks the marker dirs,
        // `live_session_holds_rotatable` reads the row's `launch_store` — so a
        // rotation taking this lock one instruction after the drop already sees
        // this session and refuses on macOS exactly as before. They are also
        // what the `has_live_session` gate on rename and disable reads, so those
        // two are refused in the gap by
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
        // The dead tree's rescue is tree-sized IO, so it waits for both holds
        // to drop. A start that failed past the rename left its tombstone for
        // the stranded-tombstone sweep.
        if let Some(tombstone) = &dead {
            rescue_tombstone(tombstone);
        }
        // Built from `paths` rather than from locals moved out of it, so the
        // runtime dir the swap repoints and the marker it must recognize as its
        // own come from one source.
        // Built from the re-read under the flock, not the caller's borrow: the
        // launch member fields (env, models, api_key) must describe what the
        // session's settings.json actually carries.
        let swap = std::sync::Arc::new(SessionSwap::new(
            session, isolation, mode, tree, &fresh, canonical, &paths,
        ));
        let SessionPaths {
            sessions, pid_file, ..
        } = paths;
        let (watchdog_signal, watchdog_handle) = spawn_session_watchdog(
            claude_home.clone(),
            std::sync::Arc::clone(&swap),
            name.as_str(),
            #[cfg(target_os = "macos")]
            seed_retry,
        )?;

        Ok(Self {
            swap,
            pid_file,
            claude_home,
            sessions,
            store_notes: notes,
            _pid_lock: pid_lock,
            watchdog_signal: Some(watchdog_signal),
            watchdog_handle: Some(watchdog_handle),
        })
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.swap.runtime
    }

    /// The notes [`build_runtime_dir_with_active_env`] raised for this build —
    /// one per shared store it could not create — for the caller to surface.
    pub(crate) fn store_notes(&self) -> &[String] {
        &self.store_notes
    }

    /// This session's registry id, for a caller that needs to edit its row after
    /// acquire (a delegate re-keys its row's pid onto the spawned child).
    pub(crate) fn session_id(&self) -> &str {
        self.swap.session.as_str()
    }

    /// Ticks this session's watchdog reconciled on rather than on a filesystem
    /// event. Test-only: the unix-only relogin test pins the event leg through
    /// this. Gated with that caller so the windows leg sees no dead code.
    #[cfg(all(test, unix))]
    pub(crate) fn tick_reconciles(&self) -> u64 {
        self.swap.tick_reconciles()
    }

    /// This session's swap executor. Production reaches it through the watchdog
    /// thread's own clone; this accessor exists so a test can drive one leg at a
    /// time instead of racing a 1 Hz tick.
    #[cfg(test)]
    fn swap(&self) -> &SessionSwap {
        &self.swap
    }

    /// This session's liveness-marker dir. Holds only its own marker under
    /// [`LinkMode::Real`] and for an isolated session under [`LinkMode::Fake`];
    /// a shared session under [`LinkMode::Fake`] still shares it with every
    /// other session of that profile+flavor, so the dir can hold several.
    ///
    /// Its live count gates anything that MOVES state out of `config_dir`: the
    /// count, not the keying, is what proves no Claude Code is reading the tree
    /// being emptied. `start::rescue_teardown` runs for isolated sessions
    /// alone, and an isolated session owns this dir under both link modes, so
    /// the count is this session alone in normal operation; the teardown's own
    /// rescue runs only on a tree its hold read a zero count for.
    pub(crate) fn sessions_dir(&self) -> &Path {
        &self.sessions
    }

    /// Take this ended session's tree off its live name, inside the teardown's
    /// hold, and drop its marker dir. A copy host's shared tree its clean final
    /// tick just published (`mirrored`) is removed outright: a rescue would
    /// walk a copy of the whole global store to find nothing. Every other tree
    /// goes through [`retire_tree`], so what only it holds (an isolated store a
    /// failed teardown rescue left, a sidecar dir `~/.claude` lacked at the
    /// build) is rescued once the hold drops, and kept if that rescue fails.
    fn retire_own_tree(&self, mirrored: bool) -> Option<PathBuf> {
        let runtime = &self.swap.runtime;
        let tombstone = if mirrored && !crate::sessions::is_symlink(runtime) {
            let _ = std::fs::remove_dir_all(runtime);
            None
        } else {
            retire_tree(runtime)
        };
        let _ = std::fs::remove_dir(&self.sessions);
        tombstone
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
    crate::testutil::set_hook(&TEARDOWN_TIMEOUT_HOOK, hook);
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
type DropRecheckHook = Box<dyn FnMut(&Path)>;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
thread_local! {
    static DROP_RECHECK_HOOK: std::cell::RefCell<Option<DropRecheckHook>> =
        const { std::cell::RefCell::new(None) };
    static DROP_HELD_MARKER: std::cell::RefCell<Option<File>> =
        const { std::cell::RefCell::new(None) };
}

/// The teardown pin seam: fires between the stop and the re-check hold, so a
/// test interposes a job/marker/lock file the re-check must re-read.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn on_drop_recheck_hold(runtime: &Path) {
    DROP_RECHECK_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(runtime);
        }
    });
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn set_drop_recheck_hook(hook: Option<DropRecheckHook>) {
    crate::testutil::set_hook(&DROP_RECHECK_HOOK, hook);
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn drop_held_marker_take() -> Option<File> {
    DROP_HELD_MARKER.with(|slot| slot.borrow_mut().take())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn drop_held_marker_set(file: File) {
    DROP_HELD_MARKER.with(|slot| *slot.borrow_mut() = Some(file));
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

        let final_tick = tick(&self.claude_home, &self.swap);
        if let Err(e) = &final_tick {
            logline!("clauth: final sync failed: {e}");
        }
        // A copy host's shared session publishes its tree through this tick's
        // mirror: a clean tick leaves nothing for a rescue to carry, and a
        // failed one leaves the tree the only home of what it did not publish,
        // so the teardown below must not touch it; the next GC sweep rescues it
        // through the tombstone path instead.
        let mirrored = self.swap.isolation == Isolation::Shared && self.swap.mode == LinkMode::Fake;
        let unpublished = final_tick.err().filter(|_| mirrored);

        // Flush this session's last `.claude.json` / `settings.json` changes to
        // the global files and siblings before a possible teardown removes this
        // runtime's copies.
        if let Err(e) = crate::claude_json::sync_once() {
            logline!("clauth: final .claude.json sync failed: {e}");
        }
        if let Err(e) = crate::settings_sync::sync_once() {
            logline!("clauth: final settings.json sync failed: {e}");
        }

        // One hold decides the collection AND, when the tree has no supervisor
        // to stop, removes it there too — the single-hold shape the flock-count
        // pin relies on. Only a tree whose supervisor reads `Ours` splits the
        // body: the verified SIGTERM + wait must run with the lock dropped (a
        // signal or wait never spans the state flock — the macOS `security`
        // rule), so its removal moves to a FRESH hold that re-checks liveness,
        // the job store AND the lock file. `unregister` and the marker release
        // stay inside the FIRST hold so the row and marker leave together. A
        // timed-out acquire is a wedged peer, not a permissions fault, so each
        // hold is retried (bounded, via `acquire_state_lock_for_teardown`).
        // The settings file is per-session (its own sid in the profile dir), so
        // it shares no state with the teardown lock below: remove it even when
        // the flock cannot be taken, so a wedged peer cannot strand it. Skip
        // while a job's `respawnFlags` name it, its tree's flags are
        // unreadable, or the store is unreadable; read the store only when the
        // file exists.
        let settings = spawn_settings_file(&self.swap.runtime, self.swap.session.as_str());
        if settings.symlink_metadata().is_ok()
            && !crate::claude_jobs::job_trees()
                .spares_settings(&settings, std::slice::from_ref(&self.swap.runtime))
        {
            let _ = std::fs::remove_file(&settings);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let mut stop = false;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let stop = false;
        let mut retired = None;
        match acquire_state_lock_for_teardown() {
            Ok(_guard) => {
                // A `--with-fallback` session a Claude Code job outlives is
                // handed on, not ended: its chain row is the record a later
                // daemon adopts. The launcher's teardown and a daemon hold's
                // release both reach here, so the same rule hands the row on
                // again while a job still names the tree, and ends it (row
                // gone, tree collected) once the job does not.
                let hand_off = !self.swap.tree.is_background()
                    && self.swap.isolation == Isolation::Shared
                    && crate::live_sessions::get(self.swap.session.as_str())
                        .is_some_and(|row| row.follows_chain)
                    && crate::claude_jobs::job_trees().spares(&self.swap.runtime);
                if !hand_off
                    && let Err(e) = crate::live_sessions::unregister(self.swap.session.as_str())
                {
                    logline!("clauth: unregistering the live session failed: {e}");
                }
                if let Err(e) = std::fs::remove_file(&self.pid_file)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    logline!("clauth: remove pid file failed: {e}");
                }
                // Every member a swap moved this session onto holds a marker of
                // its own. A dead session that keeps one blocks rotation on an
                // account nothing is using.
                self.swap.release_swapped_markers();
                let still_active = prune_stale_sessions(&self.sessions).unwrap_or(1);
                if still_active == 0
                    && let Some(e) = &unpublished
                {
                    // Spared before the supervisor match, so neither removal
                    // site runs: the direct arm below, and the post-stop
                    // re-check, which only a `stop` set inside it reaches.
                    logline!(
                        "clauth: spared runtime tree {}: its final sync failed: {e:#}",
                        self.swap.runtime.display()
                    );
                } else if still_active == 0 {
                    // `claude --bg` handed this session to Claude Code's
                    // background service before this teardown runs, so the
                    // marker alone cannot say the tree is unused: re-read the
                    // job store once and leave a tree a live job names — or one
                    // an unreadable store cannot clear — in place with its
                    // sessions dir.
                    let job_trees = crate::claude_jobs::job_trees();
                    if !job_trees.spares(&self.swap.runtime) {
                        match supervisor_state(&self.swap.runtime) {
                            SupervisorState::None | SupervisorState::Gone => {
                                retired = self.retire_own_tree(mirrored);
                            }
                            SupervisorState::Unverifiable { stale_start } => {
                                announce_supervisor_spare(
                                    &self.swap.runtime,
                                    stale_start.as_deref(),
                                );
                            }
                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            SupervisorState::Ours { .. } => {
                                // Stamp before the SIGTERM runs, so an acquire
                                // that resolves this path refuses or
                                // re-mints. A failed stamp spares the tree.
                                match record_stop_in_flight_locked(&self.swap.runtime) {
                                    Ok(()) => stop = true,
                                    Err(e) => logline!(
                                        "clauth: {}: sparing: could not record the stop in flight: {e:#}",
                                        self.swap.runtime.display()
                                    ),
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                logline!("clauth: drop cleanup failed: {e}");
                return;
            }
        }
        if let Some(tombstone) = retired.take() {
            rescue_tombstone(&tombstone);
        }
        if !stop {
            return;
        }

        // Stop the tree's supervisor with the lock dropped. A stop that cannot
        // confirm the supervisor is gone spares the tree; the next GC retries.
        // `stop_tree_supervisor` logged the outcome.
        if stop_tree_supervisor(&self.swap.runtime) == SupervisorStop::Spare {
            clear_stop_in_flight(&self.swap.runtime);
            return;
        }
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        on_drop_recheck_hold(&self.swap.runtime);

        // Re-check under a fresh lock hold, then remove: markers, the job store
        // AND the lock file, all re-read fresh.
        match acquire_state_lock_for_teardown() {
            Ok(_guard) => {
                let spares = prune_stale_sessions(&self.sessions).unwrap_or(1) != 0
                    || crate::claude_jobs::job_trees().spares(&self.swap.runtime)
                    || !matches!(
                        supervisor_state(&self.swap.runtime),
                        SupervisorState::None | SupervisorState::Gone
                    );
                if spares {
                    // Clear the in-flight record on the spare path too, or
                    // every acquire of this fixed path (the Fake shared tree,
                    // the background pair) refuses until it ages out.
                    clear_stop_in_flight(&self.swap.runtime);
                    return;
                }
                retired = self.retire_own_tree(mirrored);
            }
            Err(e) => logline!("clauth: drop cleanup failed: {e}"),
        }
        clear_stop_in_flight(&self.swap.runtime);
        if let Some(tombstone) = retired {
            rescue_tombstone(&tombstone);
        }
    }
}

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

/// Whether a process-env `key` survives [`scrub_profile_env`] into the spawned
/// child: false for every [`MANAGED_ENV_KEYS`] key (the subagent model among
/// them) and for an outgoing activation's custom env key (`stale_env_keys`),
/// true otherwise. The start demand's process-env leg derives from this same
/// definition, so a scrubbed variable never names a model the session does not
/// run.
pub(crate) fn env_survives_scrub(key: &str, stale_env_keys: &[String]) -> bool {
    !MANAGED_ENV_KEYS.contains(&key) && !stale_env_keys.iter().any(|k| k == key)
}

/// True when `dir` resolves to the real `$HOME`. `CLAUDE_CONFIG_DIR` only
/// relocates Claude Code's USER-tier settings source; the PROJECT tier is a
/// wholly separate `<cwd>/.claude/settings.json` lookup with no ancestor walk,
/// and it outranks the user tier on any key it defines. When the spawned
/// `claude`'s cwd is exactly `$HOME`, `<cwd>/.claude/` IS the real
/// `~/.claude/` — the file clauth itself writes for whichever profile is
/// globally active — so that profile's `env` silently overrides the target's.
/// Canonicalizes both sides so a symlinked `$HOME` still matches.
pub(crate) fn cwd_is_real_home(dir: &Path) -> bool {
    let Ok(home) = home_dir() else {
        return false;
    };
    match (
        crate::canonical::identity(dir),
        crate::canonical::identity(&home),
    ) {
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

/// The per-spawn `--settings` file for the CC permission exemption
/// ([`crate::cc_exempt::apply_spawn_settings`]). Lives BESIDE the runtime tree
/// (the profile dir), not inside it, so the fake-mode mirror never publishes
/// it into `~/.claude` and `settings_sync` (which reconciles only
/// `settings.json`) never reads it. Unique per session id; kept while
/// [`crate::claude_jobs::JobTrees::spares_settings`] spares it.
pub(crate) fn spawn_settings_file(config_dir: &Path, session_id: &str) -> PathBuf {
    // The name carries the session's flavor so GC can find its marker: an
    // isolated session's marker lives in `sessions-isolated-<sid>`, a shared
    // session's in `sessions-<sid>` (or the bare fake-mode `sessions`).
    let isolated = config_dir.file_name().is_some_and(|n| {
        n.to_str()
            .is_some_and(|s| s.starts_with(ISOLATED_RUNTIME_STEM))
    });
    let flavor = if isolated { "-isolated" } else { "" };
    config_dir
        .parent()
        .unwrap_or(config_dir)
        .join(format!("spawn-settings{flavor}-{session_id}.json"))
}

/// A [`Command`](std::process::Command) for the `claude` CLI — see
/// [`resolve_cli_command`] for the Windows shim story.
pub(crate) fn claude_command() -> std::process::Command {
    resolve_cli_command("claude")
}

/// The codex CLI, resolved the same way — one home for the Windows shim
/// quirk, so a second harness cannot re-learn it wrong.
pub(crate) fn codex_command() -> std::process::Command {
    resolve_cli_command("codex")
}

/// Wall-clock ceiling for waiting on a SIGTERMed supervisor to exit, chosen
/// above Claude Code's own 10 s grace (the CLI stop waits that long before it
/// would SIGKILL; the supervisor exits in ~2 s, measured, and deletes
/// its lock + status on the way out). A supervisor still alive at this bound
/// spares the tree.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SUPERVISOR_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether the tree's own supervisor may be collected past.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SupervisorState {
    /// Nothing records a supervisor: no `daemon.lock` or `daemon.status.json`
    /// regular file. A clean shutdown deletes both, so there is nothing to
    /// stop — collect.
    None,
    /// The recorded pid+start no longer names a live process: the supervisor is
    /// gone — collect.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        allow(
            dead_code,
            reason = "only linux and macOS have a positively-dead reading; Windows matches this arm but never constructs it"
        )
    )]
    Gone,
    /// A live (or unreadable) pid that is not verifiably this tree's own
    /// supervisor — spare, never signal it. `stale_start` is the recorded
    /// `procStart` when the pid DID positively read as this tree's supervisor
    /// (argv `daemon run` + config dir == tree) but its recorded start no
    /// longer equals the live one — the spare line names it.
    Unverifiable { stale_start: Option<String> },
    /// A live process verified as this tree's `claude daemon run` supervisor
    /// (linux/macOS) — SIGTERM it, then wait.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Ours { pid: u32, start: String },
}

/// `asctime(gmtime(tvsec))` in the C/POSIX locale — `"%a %b %e %H:%M:%S %Y"`,
/// day space-padded — the exact string Claude Code's macOS `daemon.lock`
/// `procStart` and `daemon.status.json` `supervisorProcStart` carry (`pbi_start_tvsec` 1792720759 -> `"Fri Oct 23 01:59:19 2026"`, measured on-device). Pure — no
/// locale, zone lookup or syscall — so it unit-tests on every target. Gated to
/// macOS + tests: only the macOS verify and the tests call it.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn format_proc_start(tvsec: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (tvsec / 86400) as i64;
    let secs = (tvsec % 86400) as i64;
    let (year, month, day) = civil_from_days(days);
    let weekday = weekday_from_days(days);
    format!(
        "{} {} {:>2} {:02}:{:02}:{:02} {}",
        WEEKDAYS[weekday as usize],
        MONTHS[(month - 1) as usize],
        day,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        year
    )
}

/// The inverse of [`format_proc_start`]: `"Fri Oct 23 01:59:19 2026"` -> epoch
/// seconds, `None` for anything else. A recorded string that does not parse is
/// a different format (a future Claude Code change) and reads as unverifiable
/// rather than as a reused pid. The weekday is checked against the date, so a
/// corrupted string cannot parse.
#[cfg(any(target_os = "macos", test))]
fn parse_proc_start(s: &str) -> Option<u64> {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let b = s.as_bytes();
    if b.len() != 24
        || b[3] != b' '
        || b[7] != b' '
        || b[10] != b' '
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b' '
    {
        return None;
    }
    let weekday = WEEKDAYS.iter().position(|w| *w == &s[0..3])?;
    let month = MONTHS.iter().position(|m| *m == &s[4..7])? as u32 + 1;
    let day: u32 = s[8..10].trim().parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let minute: i64 = s[14..16].parse().ok()?;
    let second: i64 = s[17..19].parse().ok()?;
    let year: i64 = s[20..24].parse().ok()?;
    if !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if weekday_from_days(days) as usize != weekday {
        return None;
    }
    let total = days * 86400 + hour * 3600 + minute * 60 + second;
    u64::try_from(total).ok()
}

/// The verdict when the live start does not equal the recorded `procStart`
/// string. When the live pid's argv holds `daemon run` and its
/// `CLAUDE_CONFIG_DIR` resolves to `tree`, the pid IS this tree's supervisor
/// (the recorded string was rendered by a build that spells the instant
/// differently, or in a timezone west of UTC) — spare, never collect. Otherwise
/// the recorded supervisor is dead and the pid reused: `Gone` only when the
/// recorded string parses AND the live start is strictly LATER than the
/// recorded instant — a reused pid always starts later. A string that does not
/// parse (a future format) or one that parses but is not earlier than the live
/// start reads `Unverifiable`.
#[cfg(any(target_os = "macos", test))]
fn start_mismatch_verdict(
    live_start_tvsec: u64,
    recorded: &str,
    supervisor_matches: bool,
) -> SupervisorState {
    if supervisor_matches {
        return SupervisorState::Unverifiable {
            stale_start: Some(recorded.to_string()),
        };
    }
    match parse_proc_start(recorded) {
        Some(recorded) if live_start_tvsec > recorded => SupervisorState::Gone,
        Some(_) | None => SupervisorState::Unverifiable { stale_start: None },
    }
}

/// Days since 1970-01-01 to (year, month, day) — Howard Hinnant's
/// `civil_from_days`, all-integer, valid over the whole signed range.
#[cfg(any(target_os = "macos", test))]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Weekday of `days` since 1970-01-01, 0 = Sunday. 1970-01-01 was a Thursday.
#[cfg(any(target_os = "macos", test))]
fn weekday_from_days(days: i64) -> u8 {
    (days + 4).rem_euclid(7) as u8
}

/// (year, month, day) to days since 1970-01-01 — Hinnant's `days_from_civil`.
#[cfg(any(target_os = "macos", test))]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = (if month > 2 { month - 3 } else { month + 9 }) as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// A macOS `KERN_PROCARGS2` buffer parsed into argv and the env tail. Each
/// argv entry is an owned NUL-terminated byte string; the env tail stays raw
/// (NUL-separated) so the config-dir comparison reads it the same way linux
/// reads `/proc/<pid>/environ`.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
struct ProcArgs {
    argv: Vec<Vec<u8>>,
    environ: Vec<u8>,
}

/// The NUL-terminated byte string starting at `offset`: `(bytes, next_offset)`,
/// `None` when the buffer ends before a NUL.
#[cfg(any(target_os = "macos", test))]
fn nul_string_at(buf: &[u8], offset: usize) -> Option<(&[u8], usize)> {
    let end = offset + buf.get(offset..)?.iter().position(|b| *b == 0)?;
    Some((&buf[offset..end], end + 1))
}

/// Parse a `KERN_PROCARGS2` byte buffer: `[int argc][exec path NUL][NUL
/// padding][argc argv strings NUL][env strings NUL]`, all native-endian. Pure
/// — no syscall — so it unit-tests on every OS. `None` on a truncated buffer,
/// an `argc` past the end, or an argv string that never terminates. An empty
/// `argv[0]` is consumed by the padding skip (it is just a NUL), which shifts
/// argv by one and then fails closed.
#[cfg(any(target_os = "macos", test))]
fn parse_proc_args2(buf: &[u8]) -> Option<ProcArgs> {
    let argc = i32::from_ne_bytes(buf.get(0..4)?.try_into().ok()?) as usize;
    let mut offset = 4;
    // Skip the exec path (NUL-terminated).
    while *buf.get(offset)? != 0 {
        offset += 1;
    }
    offset += 1;
    // Skip the NUL padding between the exec path and argv[0].
    while buf.get(offset) == Some(&0) {
        offset += 1;
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        let (arg, next) = nul_string_at(buf, offset)?;
        argv.push(arg.to_vec());
        offset = next;
    }
    Some(ProcArgs {
        argv,
        environ: buf[offset..].to_vec(),
    })
}

/// Whether argv holds `daemon` immediately followed by `run` — the transient
/// supervisor's argv shape. The one check both the linux `/proc/<pid>/cmdline`
/// read and the macOS `KERN_PROCARGS2` argv parse use.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn argv_holds_daemon_run(args: &[&[u8]]) -> bool {
    args.windows(2)
        .any(|pair| pair == [b"daemon".as_slice(), b"run".as_slice()])
}

/// Whether NUL-separated env bytes hold `CLAUDE_CONFIG_DIR=<value>` whose
/// canonical path equals `tree`'s — the one comparison the linux
/// `/proc/<pid>/environ` read and the macOS `KERN_PROCARGS2` env tail share,
/// so the two arms cannot drift apart. A missing entry, a non-UTF-8 value, or
/// a value that does not resolve to `tree` all read false (unverifiable).
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn environ_bytes_config_dir_is(environ: &[u8], tree: &Path) -> bool {
    let Some(ccd) = environ
        .split(|b| *b == 0)
        .find_map(|kv| kv.strip_prefix(b"CLAUDE_CONFIG_DIR="))
    else {
        return false;
    };
    let Ok(ccd) = std::str::from_utf8(ccd) else {
        return false;
    };
    match (
        crate::canonical::identity(tree),
        crate::canonical::identity(Path::new(ccd)),
    ) {
        (Ok(tree), Ok(ccd)) => tree == ccd,
        _ => false,
    }
}

/// The process `pid`'s environment as NUL-separated `KEY=value` bytes, read as
/// the supervisor checks read it: `/proc/<pid>/environ` on linux, the
/// `KERN_PROCARGS2` env tail on macOS. `None` where it cannot be read, and on
/// every other platform.
pub(crate) fn process_environ(pid: u32) -> Option<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read(format!("/proc/{pid}/environ")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        macos_proc_args2(pid).map(|args| args.environ)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Whether the process `pid` is Claude Code's background service, by the
/// supervisor's argv (`claude daemon run`), read as the supervisor checks read
/// it. False where argv cannot be read, and on every other platform.
pub(crate) fn process_is_claude_service(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        cmdline_holds_daemon_run(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_proc_args2(pid).is_some_and(|args| {
            let argv: Vec<&[u8]> = args.argv.iter().map(Vec::as_slice).collect();
            argv_holds_daemon_run(&argv)
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        false
    }
}

/// Whether NUL-separated env bytes set `key` to a non-empty value.
pub(crate) fn environ_sets(environ: &[u8], key: &str) -> bool {
    environ.split(|b| *b == 0).any(|kv| {
        kv.strip_prefix(key.as_bytes())
            .and_then(|rest| rest.strip_prefix(b"="))
            .is_some_and(|value| !value.is_empty())
    })
}

/// `daemon.lock`'s `pid` + `procStart`. The lock is a CONTENT file, not a
/// kernel lock (the supervisor holds zero flocks and no fd on it, measured):
/// the pid-reuse-safe identity is the `(pid, procStart)` pair.
#[derive(serde::Deserialize)]
struct DaemonLock {
    pid: u32,
    #[serde(rename = "procStart")]
    proc_start: String,
}

/// `daemon.status.json`'s `supervisorPid` + `supervisorProcStart`, the
/// fallback when the lock is absent (the supervisor deletes both on a clean
/// shutdown; the roster's `supervisorPid` carries no start time).
#[derive(serde::Deserialize)]
struct DaemonStatus {
    #[serde(rename = "supervisorPid")]
    supervisor_pid: u32,
    #[serde(rename = "supervisorProcStart")]
    supervisor_proc_start: String,
}

/// Whether a recorded supervisor pid is usable as a `kill` target: it must be
/// a positive process id that fits `pid_t`. `0` makes `kill(0, …)` probe the
/// caller's own process group, and a value above `i32::MAX` casts to a
/// negative pid (`4294967295` -> `kill(-1, …)`, every process the user owns).
/// Both files are written by another program, so a pid outside `1..=i32::MAX`
/// is a malformed record — spare as unverifiable, never signal.
fn supervisor_record_pid_is_valid(pid: u32) -> bool {
    pid != 0 && pid <= i32::MAX as u32
}

/// `(pid, proc_start)` from `<tree>/daemon.lock`, falling back to
/// `<tree>/daemon.status.json`. `Ok(None)`: neither is a regular file.
/// `Err(())`: a non-regular, non-symlink entry (a directory), a regular
/// file that failed to parse, or a record whose pid is 0 or above `i32::MAX`
/// — fail closed, a supervisor may be alive and unverifiable.
///
/// A SYMLINK reads as absent: an older clauth build linked the home's
/// lock/status into the tree, so the supervisor the link names is the HOME
/// roster's, never this tree's own — the tree collects with no stop, and
/// `remove_dir_all` unlinks the link, leaving the shared target intact.
fn read_supervisor_record(tree: &Path) -> Result<Option<(u32, String)>, ()> {
    let lock = tree.join("daemon.lock");
    match lock.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() => {
            let body = std::fs::read(&lock).map_err(|_| ())?;
            let record = serde_json::from_slice::<DaemonLock>(&body).map_err(|_| ())?;
            if !supervisor_record_pid_is_valid(record.pid) {
                return Err(());
            }
            return Ok(Some((record.pid, record.proc_start)));
        }
        Ok(meta) if meta.file_type().is_symlink() => {}
        Ok(_) => return Err(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }
    let status = tree.join("daemon.status.json");
    match status.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() => {
            let body = std::fs::read(&status).map_err(|_| ())?;
            let record = serde_json::from_slice::<DaemonStatus>(&body).map_err(|_| ())?;
            if !supervisor_record_pid_is_valid(record.supervisor_pid) {
                return Err(());
            }
            Ok(Some((record.supervisor_pid, record.supervisor_proc_start)))
        }
        Ok(meta) if meta.file_type().is_symlink() => Ok(None),
        Ok(_) => Err(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// What the tree's supervisor files say, before any signal is sent. No
/// subprocess and no wait — so this is safe inside the state flock (the
/// acquire's consult runs it there).
fn supervisor_state(tree: &Path) -> SupervisorState {
    match read_supervisor_record(tree) {
        Ok(Some((pid, start))) => verify_supervisor_record(tree, pid, &start),
        // No lock or status file: the supervisor shut down cleanly (it deletes
        // both on the way out, measured), so there is no recorded
        // pid to stop. The roster's `supervisorPid` is NOT a fallback: it
        // carries no start time, so a stale pid it names could be reused by any
        // live process and read as an unverifiable supervisor forever.
        Ok(None) => SupervisorState::None,
        Err(()) => SupervisorState::Unverifiable { stale_start: None },
    }
}

#[cfg(target_os = "linux")]
fn verify_supervisor_record(tree: &Path, pid: u32, start: &str) -> SupervisorState {
    // kill(0) proves the pid still names a process — live, zombie, or a
    // hidepid-hidden one — without reading /proc. False means the pid is
    // positively gone (only that counts as dead, never an unreadable or
    // hidden `/proc/<pid>/stat`).
    if !namespaced_keychain_ledger::pid_alive(pid) {
        return SupervisorState::Gone;
    }
    match read_proc_stat(pid) {
        // SIGTERM landed and the supervisor is dead-but-unreaped: gone.
        ProcStat::Zombie => SupervisorState::Gone,
        // The pid is alive (kill 0 above) but its identity cannot be read:
        // spare, never signal and never collect.
        ProcStat::Unreadable => SupervisorState::Unverifiable { stale_start: None },
        ProcStat::Live(actual) if actual == start => {
            if !supervisor_argv_and_config_dir_match(pid, tree) {
                return SupervisorState::Unverifiable { stale_start: None };
            }
            SupervisorState::Ours {
                pid,
                start: start.to_string(),
            }
        }
        // A live process with a different start. Read argv + env first: a pid
        // whose argv holds `daemon run` and whose config dir is this tree is
        // this tree's supervisor with a stale recorded start — spare, never
        // collect a live supervisor it would recreate. Only otherwise is the
        // recorded supervisor gone and the pid reused — collect, never signal.
        ProcStat::Live(_) => {
            if supervisor_argv_and_config_dir_match(pid, tree) {
                SupervisorState::Unverifiable {
                    stale_start: Some(start.to_string()),
                }
            } else {
                SupervisorState::Gone
            }
        }
    }
}

/// The pid's `/proc/<pid>/stat` state for the verify path, where a zombie must
/// read as GONE (its SIGTERM landed) and an unreadable stat as unverifiable —
/// two outcomes [`crate::daemon::gateway::process_start_time`] collapses into
/// one `None`.
#[cfg(target_os = "linux")]
enum ProcStat {
    /// Dead-but-unreaped (state `Z`/`X`/`x`): the process is gone, its
    /// `/proc` entry lingers only until its parent reaps it.
    Zombie,
    /// The stat is present but cannot be read or parsed (hidepid, `EACCES`,
    /// `EMFILE`, a malformed line): identity unverifiable — spare.
    Unreadable,
    /// A live (non-zombie) process with its field-22 start time.
    Live(String),
}

#[cfg(target_os = "linux")]
fn read_proc_stat(pid: u32) -> ProcStat {
    #[cfg(all(test, target_os = "linux"))]
    if forced_stat_read_failure(pid) {
        return ProcStat::Unreadable;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return ProcStat::Unreadable;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return ProcStat::Unreadable;
    };
    let mut fields = rest.split_whitespace();
    match fields.next() {
        None => return ProcStat::Unreadable,
        Some("Z" | "X" | "x") => return ProcStat::Zombie,
        Some(_) => {}
    }
    match fields.nth(18) {
        Some(start) => ProcStat::Live(start.to_string()),
        None => ProcStat::Unreadable,
    }
}

// Test-only seam: force [`read_proc_stat`] to read one pid's `/proc` entry
// as unreadable, so a test can pose the hidepid/EACCES arm deterministically —
// a live process's stat is always readable to its parent on this box, so no
// fixture can produce a genuine unreadable read. The countdown lets a test
// fail only the POST-signal reads (the verify's first read succeeds, the
// wait's polls fail), which is what exercises the liveness reader's own
// `ProcStat::Unreadable` mapping.
#[cfg(all(test, target_os = "linux"))]
thread_local! {
    // (pid, successful reads left before failing): every later read for `pid`
    // returns `Unreadable`.
    static FORCED_UNREADABLE_STAT: std::cell::Cell<Option<(u32, u32)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, target_os = "linux"))]
fn forced_stat_read_failure(pid: u32) -> bool {
    FORCED_UNREADABLE_STAT.with(|c| {
        let Some((armed_pid, reads_left)) = c.get() else {
            return false;
        };
        if armed_pid != pid {
            return false;
        }
        if reads_left == 0 {
            return true;
        }
        c.set(Some((armed_pid, reads_left - 1)));
        false
    })
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) struct UnreadableProcStatGuard(Option<(u32, u32)>);

#[cfg(all(test, target_os = "linux"))]
impl UnreadableProcStatGuard {
    /// Fail every [`read_proc_stat`] read for `pid` (the verify path's seam).
    pub(crate) fn set(pid: u32) -> Self {
        Self::after(pid, 0)
    }

    /// Fail [`read_proc_stat`] reads for `pid` only after `reads` successful
    /// reads (the post-SIGTERM liveness seam: the verify's read succeeds, the
    /// wait's polls fail).
    pub(crate) fn after(pid: u32, reads: u32) -> Self {
        let prev = FORCED_UNREADABLE_STAT.with(|c| c.replace(Some((pid, reads))));
        Self(prev)
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for UnreadableProcStatGuard {
    fn drop(&mut self) {
        FORCED_UNREADABLE_STAT.with(|c| c.set(self.0));
    }
}

#[cfg(target_os = "macos")]
fn verify_supervisor_record(tree: &Path, pid: u32, start: &str) -> SupervisorState {
    // macOS: `procStart` is `asctime(gmtime(pbi_start_tvsec))` — read the live
    // pid's `pbi_start_tvsec` via `proc_pidinfo` and compare by equality with
    // the recorded string, then verify argv + `CLAUDE_CONFIG_DIR` in-process
    // from `KERN_PROCARGS2`. A start in a format we do not render (a future
    // Claude Code change), a missing or mismatched config dir, or an unreadable
    // argv/env buffer is unverifiable -> spare; the same format with a strictly
    // later live instant is a reused pid -> gone.
    if !namespaced_keychain_ledger::pid_alive(pid) {
        return SupervisorState::Gone;
    }
    let Some(info) = proc_bsdinfo(pid) else {
        // Live (kill 0 above) but unreadable through `proc_pidinfo`: fall back
        // to the sysctl `kinfo_proc` read. Only a sysctl-confirmed zombie or a
        // pid the sysctl reports absent reads Gone; anything else spares.
        return match proc_kinfo_liveness(pid) {
            SupervisorLiveness::Gone => SupervisorState::Gone,
            SupervisorLiveness::Alive | SupervisorLiveness::Unreadable => {
                SupervisorState::Unverifiable { stale_start: None }
            }
        };
    };
    if info.pbi_status == libc::SZOMB {
        // SIGTERM landed and the supervisor is dead-but-unreaped: gone.
        return SupervisorState::Gone;
    }
    if format_proc_start(info.pbi_start_tvsec) != start {
        // Read argv + env first: a pid whose argv holds `daemon run` and whose
        // config dir is this tree is this tree's supervisor (the recorded
        // string was rendered differently, or in a timezone west of UTC) —
        // spare, never collect a live supervisor it would recreate.
        return start_mismatch_verdict(
            info.pbi_start_tvsec,
            start,
            supervisor_argv_and_config_dir_match(pid, tree),
        );
    }
    let Some(args) = macos_proc_args2(pid) else {
        return SupervisorState::Unverifiable { stale_start: None };
    };
    let argv: Vec<&[u8]> = args.argv.iter().map(Vec::as_slice).collect();
    if !argv_holds_daemon_run(&argv) || !environ_bytes_config_dir_is(&args.environ, tree) {
        return SupervisorState::Unverifiable { stale_start: None };
    }
    SupervisorState::Ours {
        pid,
        start: start.to_string(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn verify_supervisor_record(tree: &Path, pid: u32, start: &str) -> SupervisorState {
    let _ = (tree, pid, start);
    // Windows: the supervisor's identity form is unmeasured, so a recorded pid
    // can never be verified as positively dead — the lock/status record spares,
    // never collects, and clauth never signals. A tree whose supervisor shut
    // down cleanly deletes both files, so it still collects via `None`.
    SupervisorState::Unverifiable { stale_start: None }
}

/// The live pid's BSD process info, or `None` when the kernel refuses (the pid
/// is gone or unreadable). The one `proc_pidinfo(PROC_PIDTBSDINFO)` read,
/// shared by the supervisor verify here and `session_record::parent_pid`.
#[cfg(target_os = "macos")]
pub(crate) fn proc_bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    #[allow(unsafe_code)]
    // SAFETY: `proc_bsdinfo` is all integers and fixed-size `c_char` arrays, so
    // the zeroed value is valid and no field is read before `proc_pidinfo`
    // fills the struct.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    // SAFETY: `proc_pidinfo` copies the BSD process info for `pid` into `info`,
    // which we own; the size passed is that struct's.
    let rc = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    (rc >= std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int).then_some(info)
}

/// `extern_proc.p_stat`'s `SZOMB` value (`<sys/proc.h>`): a zombie. libc's
/// Apple `SZOMB` is a `u32`, but `p_stat` is the `char` the `kinfo_proc` read
/// returns, so the constant lives here as a `u8` so the pure decision
/// unit-tests on every OS.
#[cfg(any(target_os = "macos", test))]
const P_STAT_SZOMB: u8 = 5;

/// The `sysctl([CTL_KERN, KERN_PROC, KERN_PROC_PID, pid])` outcome, split so
/// the decision over it unit-tests on every OS: `Present(p_stat)` (a pid the
/// kernel knows, possibly a zombie), `Absent` (no such pid), or `Unreadable`
/// (the read failed for some other reason).
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KinfoProcRead {
    Present(u8),
    Absent,
    Unreadable,
}

/// Whether a `kinfo_proc` read proves the pid is GONE: only a `SZOMB` zombie
/// or a pid the sysctl reports absent. Any live non-zombie state and every
/// other read failure stays unreadable (spare).
#[cfg(any(target_os = "macos", test))]
fn kinfo_proc_reads_gone(read: KinfoProcRead) -> bool {
    match read {
        KinfoProcRead::Absent => true,
        KinfoProcRead::Present(P_STAT_SZOMB) => true,
        KinfoProcRead::Present(_) | KinfoProcRead::Unreadable => false,
    }
}

/// The `kinfo_proc` prefix whose only field the supervisor read needs:
/// `kp_proc.p_stat` at [`P_STAT_OFFSET`]. libc binds no macOS `kinfo_proc`, so
/// this spells the `extern_proc` fields before `p_stat` (`<sys/proc.h>`:
/// `[p_un: 16][p_vmspace: 8][p_sigacts: 8][p_flag: 4][p_stat: 1]...`) and
/// [`P_STAT_OFFSET`] is derived with `offset_of!`, never a bare literal.
#[cfg(any(target_os = "macos", test))]
#[repr(C)]
struct KinfoProcStat {
    _p_un: [usize; 2],
    _p_vmspace: usize,
    _p_sigacts: usize,
    _p_flag: libc::c_int,
    p_stat: libc::c_char,
}

#[cfg(any(target_os = "macos", test))]
const P_STAT_OFFSET: usize = std::mem::offset_of!(KinfoProcStat, p_stat);

/// Decide the `kinfo_proc` outcome from the second-pass read: `size` is the
/// byte count that call reported, so the first pass's larger buffer tail is
/// not part of the read. An empty read (the pid vanished between the sizing
/// call and the read, so the kernel wrote no entry) is `Absent`; a read
/// shorter than the `KinfoProcStat` prefix (40 bytes) is `Unreadable`; a
/// full-enough read reports the `p_stat` byte.
#[cfg(any(target_os = "macos", test))]
fn kinfo_proc_second_read(buf: &[u8], size: usize) -> KinfoProcRead {
    // A reported size past the allocation is not a read of this buffer.
    let Some(read) = buf.get(..size) else {
        return KinfoProcRead::Unreadable;
    };
    if read.is_empty() {
        return KinfoProcRead::Absent;
    }
    if read.len() < std::mem::size_of::<KinfoProcStat>() {
        return KinfoProcRead::Unreadable;
    }
    read.get(P_STAT_OFFSET)
        .copied()
        .map_or(KinfoProcRead::Unreadable, KinfoProcRead::Present)
}

/// Read `kinfo_proc.kp_proc.p_stat` for `pid` through
/// `sysctl([CTL_KERN, KERN_PROC, KERN_PROC_PID, pid])` — the read that still
/// sees a dead-but-unreaped process, where `proc_pidinfo(PROC_PIDTBSDINFO)`
/// returns no data. The pid vanishing between the sizing call and the read
/// empties the second result (`Absent`); XNU's `KERN_PROC_PID` handler never
/// fails with `ESRCH`, and any other failure or a too-short read is
/// `Unreadable`.
#[cfg(target_os = "macos")]
fn proc_kinfo_read(pid: u32) -> KinfoProcRead {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut size: libc::size_t = 0;
    #[allow(unsafe_code)]
    // SAFETY: `mib` is a valid four-element `c_int` array; `sysctl` reads it
    // and, with a NULL oldp, writes only the required byte count into `size`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return KinfoProcRead::Unreadable;
    }
    let mut buf = vec![0u8; size];
    #[allow(unsafe_code)]
    // SAFETY: `buf` is a `size`-byte owned allocation and `sysctl` writes at
    // most `*oldlenp` bytes into it; `mib` is a valid `c_int` array.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return KinfoProcRead::Unreadable;
    }
    kinfo_proc_second_read(&buf, size)
}

/// The liveness verdict for a pid `proc_bsdinfo` could not read but
/// `pid_alive` still finds — the shared fallback the verify and the liveness
/// poll run. Only a sysctl-confirmed zombie or an absent pid reads `Gone`.
#[cfg(target_os = "macos")]
fn proc_kinfo_liveness(pid: u32) -> SupervisorLiveness {
    if kinfo_proc_reads_gone(proc_kinfo_read(pid)) {
        SupervisorLiveness::Gone
    } else {
        SupervisorLiveness::Unreadable
    }
}

/// The pid's argv + environment read in-process via
/// `sysctl([CTL_KERN, KERN_PROCARGS2, pid])` — the kernel's NUL-separated
/// layout, so a space inside an argument or a value cannot fool the parse and
/// nothing shells out (the same shape linux's `/proc/<pid>/cmdline` + environ
/// read uses). `None` on an unreadable buffer or a parse failure: unverifiable.
#[cfg(target_os = "macos")]
fn macos_proc_args2(pid: u32) -> Option<ProcArgs> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    // First call with a NULL oldp yields the byte size the kernel would write.
    let mut size: libc::size_t = 0;
    #[allow(unsafe_code)]
    // SAFETY: `mib` is a valid three-element `c_int` array; `sysctl` reads it
    // and writes only `size` (the oldp argument is NULL, so no buffer).
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    #[allow(unsafe_code)]
    // SAFETY: `buf` is a `size`-byte owned allocation, `sysctl` writes at most
    // `*oldlenp` bytes into it, and `mib` is a valid `c_int` array.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    parse_proc_args2(&buf)
}

/// Whether the process `pid` holds the transient supervisor's argv AND its
/// config dir resolves to `tree` — the same two checks the equality arm runs,
/// factored so the start-mismatch arm can read them first.
#[cfg(target_os = "macos")]
fn supervisor_argv_and_config_dir_match(pid: u32, tree: &Path) -> bool {
    let Some(args) = macos_proc_args2(pid) else {
        return false;
    };
    let argv: Vec<&[u8]> = args.argv.iter().map(Vec::as_slice).collect();
    argv_holds_daemon_run(&argv) && environ_bytes_config_dir_is(&args.environ, tree)
}

/// Whether the process `pid`'s cmdline holds `daemon` immediately followed by
/// `run` — the transient supervisor's argv shape (`claude daemon run --origin
/// transient`, measured).
#[cfg(target_os = "linux")]
fn cmdline_holds_daemon_run(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    argv_holds_daemon_run(&args)
}

/// Whether the process `pid`'s `CLAUDE_CONFIG_DIR` resolves to the same
/// canonical path as `tree` — the supervisor is keyed on its config dir, so a
/// mismatch is another tree's supervisor.
#[cfg(target_os = "linux")]
fn environ_config_dir_is(pid: u32, tree: &Path) -> bool {
    let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    environ_bytes_config_dir_is(&environ, tree)
}

/// Whether the process `pid` holds the transient supervisor's argv AND its
/// config dir resolves to `tree` — the two checks that say the pid is this
/// tree's supervisor, shared by the equality and start-mismatch verify arms.
#[cfg(target_os = "linux")]
fn supervisor_argv_and_config_dir_match(pid: u32, tree: &Path) -> bool {
    cmdline_holds_daemon_run(pid) && environ_config_dir_is(pid, tree)
}

/// Whether collecting the tree may proceed past its supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorStop {
    Proceed,
    Spare,
}

/// Stop the tree's OWN supervisor by the pid its lock/status names, verified
/// against that pid's start time, argv and config dir — never through the
/// `claude daemon stop` CLI, which would also stop an installed home service
/// and every tree it hosts. Runs OUTSIDE the state flock; the callers hold the
/// decision's `SupervisorState::Ours` from a prior flock hold. A supervisor
/// that is gone (or never existed) proceeds; anything unverifiable or still
/// alive at the deadline spares the tree.
fn stop_tree_supervisor(tree: &Path) -> SupervisorStop {
    debug_assert!(
        !crate::lockorder::holds::<crate::lockorder::rank::State>(),
        "the supervisor stop (SIGTERM + wait) must run outside the state flock"
    );
    match supervisor_state(tree) {
        SupervisorState::None | SupervisorState::Gone => SupervisorStop::Proceed,
        SupervisorState::Unverifiable { stale_start } => {
            announce_supervisor_spare(tree, stale_start.as_deref());
            SupervisorStop::Spare
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        SupervisorState::Ours { pid, start } => {
            // A supervisor that wedged on a previous pass is spared now without
            // another SIGTERM + wait; the wedged record ages out after
            // [`WEDGED_STOP_SKIP_AFTER`], so the stop is retried, not dropped.
            if wedged_stop_is_fresh(tree, pid, &start) {
                if supervisor_spare_should_announce(tree) {
                    logline!(
                        "clauth: {}: sparing: its supervisor pid {pid} already refused to stop; not re-signalling yet",
                        tree.display()
                    );
                }
                SupervisorStop::Spare
            } else {
                signal_and_wait(tree, pid, &start)
            }
        }
    }
}

/// Every `runtime*` dir directly under a profile dir, by the strict predicate
/// [`gc_runtime_trees`] deletes with. A delete/rename removes the whole
/// profile dir, so it must stop each tree's own supervisor first; the
/// enumeration shares the delete-shaped predicate so a future
/// `runtime_state.json` never enters the stop set.
fn runtime_trees_under(profile_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(profile_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_runtime_dir_name)
        })
        .collect()
}

/// Whether a delete/rename caller offers `--force` as the way past a
/// supervisor refusal. The CLI's delete does — `--force` still stops every verifiable
/// supervisor and waives the refusal for any supervisor it cannot stop
/// (unverifiable, or verified but still alive after SIGTERM); rename has no
/// such flag, so its refusal names only the retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisorStopGate {
    /// `--force` was passed: stop every verifiable supervisor, remove every
    /// tree anyway (an unverifiable one, or one verified but still alive
    /// after SIGTERM, spares with no signal or a skipped re-wait).
    Forced,
    /// No `--force`: stop each tree; refuse naming the tree. `offers_force`
    /// selects the message tail — the CLI's delete appends the `--force`
    /// clause; rename and the TUI's delete (which have no such flag) do not.
    Refused { offers_force: bool },
}

/// Stop every runtime tree's own supervisor under `profile_dir`, before a
/// delete/rename removes the whole profile dir. Reuses
/// [`stop_tree_supervisor`] — the one stop function — and, under
/// [`SupervisorStopGate::Refused`], refuses with an error naming the tree when
/// a stop cannot be confirmed (an unverifiable supervisor, one still alive at
/// the deadline, or one that appeared between the stop and the re-check). The
/// caller holds the profile's rotation guard, so no acquire can re-mint a path
/// during the stop; the stop itself runs outside the state flock
/// (`stop_tree_supervisor`'s own assertion).
///
/// [`SupervisorStopGate::Forced`] still runs `stop_tree_supervisor` for every
/// tree — a verifiable supervisor is SIGTERMed and waited on, and an
/// unverifiable one spares without a signal — then removes the tree anyway: the
/// operator said remove it, but a verified stop is never skipped, so a forced
/// delete of a live linux/macOS supervisor cannot leak it (it would recreate
/// its config dir bare under the deleted path). The refusal is not stamped into
/// the in-flight journal: the rotation guard the caller holds keeps an acquire
/// from re-minting the path during the stop, and a concurrent GC's stop of the
/// same supervisor is idempotent (the second SIGTERM reads ESRCH).
pub(crate) fn stop_profile_tree_supervisors(
    name: &ProfileName,
    profile_dir: &Path,
    gate: SupervisorStopGate,
) -> Result<()> {
    match gate {
        SupervisorStopGate::Refused { offers_force } => {
            for tree in runtime_trees_under(profile_dir) {
                let stop = stop_tree_supervisor(&tree);
                #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
                on_profile_stop_recheck_hold(&tree);
                let spare = stop == SupervisorStop::Spare || {
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    {
                        !supervisor_gone_recheck(&tree)
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                    {
                        false
                    }
                };
                if spare {
                    let tail = if offers_force {
                        ", or pass --force to remove it anyway"
                    } else {
                        ""
                    };
                    anyhow::bail!(
                        "'{name}': runtime tree {} still has a background supervisor \
                         clauth cannot stop on this host; retry once it has exited{tail}",
                        tree.display()
                    );
                }
            }
            Ok(())
        }
        SupervisorStopGate::Forced => {
            for tree in runtime_trees_under(profile_dir) {
                let _ = stop_tree_supervisor(&tree);
            }
            Ok(())
        }
    }
}

/// Re-check under a fresh flock hold that the tree's supervisor is gone, the
/// same shape GC and teardown run between the stop and the removal: a
/// supervisor that is still there (or reappeared) spares the tree. Fail-closed
/// — a flock the re-check cannot take reads as a spare.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn supervisor_gone_recheck(tree: &Path) -> bool {
    with_state_lock(|_held| {
        Ok(matches!(
            supervisor_state(tree),
            SupervisorState::None | SupervisorState::Gone
        ))
    })
    .unwrap_or(false)
}

/// Test-only seam: fires between the stop and the delete/rename re-check hold,
/// so a test interposes a supervisor the re-check must re-read. Linux/macOS
/// only — the tests that pose it need a verifiable supervisor, which Windows
/// never has.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) type ProfileStopRecheckHook = Box<dyn FnMut(&Path)>;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
thread_local! {
    static PROFILE_STOP_RECHECK_HOOK: std::cell::RefCell<Option<ProfileStopRecheckHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn on_profile_stop_recheck_hold(tree: &Path) {
    PROFILE_STOP_RECHECK_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(tree);
        }
    });
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) fn set_profile_stop_recheck_hook(hook: Option<ProfileStopRecheckHook>) {
    crate::testutil::set_hook(&PROFILE_STOP_RECHECK_HOOK, hook);
}

/// Test-only seam: fires between the verify and the SIGTERM, so a test can
/// pose the supervisor exiting in that window — the kill then reads `ESRCH`,
/// the idempotent-stop case. Linux/macOS only, the same verifiable-supervisor
/// reason as [`ProfileStopRecheckHook`].
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) type SignalSendHook = Box<dyn FnMut(u32)>;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
thread_local! {
    static SIGNAL_SEND_HOOK: std::cell::RefCell<Option<SignalSendHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn on_signal_send_hook(pid: u32) {
    SIGNAL_SEND_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(pid);
        }
    });
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) fn set_signal_send_hook(hook: Option<SignalSendHook>) {
    crate::testutil::set_hook(&SIGNAL_SEND_HOOK, hook);
}

/// Whether the recorded `(pid, start)` still names the supervisor, or is gone
/// or unreadable. Linux reads field 22 of `/proc/<pid>/stat`; macOS reads the
/// live `proc_bsdinfo` and renders `format_proc_start` — both in-process, no
/// subprocess per poll. Only a positively-gone pid (or a different start — the
/// pid was reused) reads `Gone`; a failed read reads `Unreadable`, never gone.
#[cfg(any(target_os = "linux", target_os = "macos"))]
enum SupervisorLiveness {
    /// The recorded supervisor is gone: the pid no longer exists (or is a
    /// zombie) or its start time differs.
    Gone,
    /// The pid is live at the recorded start.
    Alive,
    /// The read failed: neither gone nor alive can be asserted.
    Unreadable,
}

#[cfg(target_os = "linux")]
fn supervisor_liveness_at_start(pid: u32, start: &str) -> SupervisorLiveness {
    if !namespaced_keychain_ledger::pid_alive(pid) {
        return SupervisorLiveness::Gone;
    }
    match read_proc_stat(pid) {
        ProcStat::Zombie => SupervisorLiveness::Gone,
        ProcStat::Unreadable => SupervisorLiveness::Unreadable,
        ProcStat::Live(actual) if actual == start => SupervisorLiveness::Alive,
        ProcStat::Live(_) => SupervisorLiveness::Gone,
    }
}

#[cfg(target_os = "macos")]
fn supervisor_liveness_at_start(pid: u32, start: &str) -> SupervisorLiveness {
    if !namespaced_keychain_ledger::pid_alive(pid) {
        return SupervisorLiveness::Gone;
    }
    let Some(info) = proc_bsdinfo(pid) else {
        // Live (kill 0 above) but unreadable through `proc_pidinfo`: fall back
        // to the sysctl `kinfo_proc` read. Only a sysctl-confirmed zombie or a
        // pid the sysctl reports absent reads Gone.
        return proc_kinfo_liveness(pid);
    };
    if info.pbi_status == libc::SZOMB {
        return SupervisorLiveness::Gone;
    }
    if format_proc_start(info.pbi_start_tvsec) == start {
        SupervisorLiveness::Alive
    } else {
        SupervisorLiveness::Gone
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(
    unsafe_code,
    reason = "signal delivery: SIGTERM to a pid verified as this tree's supervisor"
)]
fn signal_and_wait(tree: &Path, pid: u32, start: &str) -> SupervisorStop {
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    on_signal_send_hook(pid);
    // SAFETY: `pid` was verified as this tree's `claude daemon run` supervisor
    // just above; SIGTERM asks it to shut down (it exits in ~2 s and
    // deletes its lock + status on the way out, measured).
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if sent != 0 {
        let e = std::io::Error::last_os_error();
        // The verified supervisor exited between the verify and the kill (a
        // concurrent stop landed first, or an idle supervisor shut down there):
        // that IS the stopped outcome, and the caller's re-check re-reads the
        // lock file before removing anything, so proceed. Every other errno
        // keeps the wedged row and spares.
        if e.raw_os_error() == Some(libc::ESRCH) {
            return SupervisorStop::Proceed;
        }
        record_wedged_stop(tree, pid, start);
        if supervisor_spare_should_announce(tree) {
            logline!(
                "clauth: {}: sparing: could not SIGTERM its supervisor pid {pid}: {e}",
                tree.display()
            );
        }
        return SupervisorStop::Spare;
    }
    let deadline = Instant::now() + supervisor_stop_deadline();
    let mut seen_alive = false;
    loop {
        match supervisor_liveness_at_start(pid, start) {
            SupervisorLiveness::Gone => return SupervisorStop::Proceed,
            SupervisorLiveness::Alive => seen_alive = true,
            SupervisorLiveness::Unreadable => {}
        }
        if Instant::now() >= deadline {
            // The verdict is the WHOLE wait's reading: any poll that saw the
            // supervisor alive means it wedged (SIGTERM landed and it is still
            // there), so the wedged row and the "still alive" line only yield
            // to a wait where no poll ever read it alive — that one records no
            // wedged row, so a later pass retries the full stop instead of
            // skipping a supervisor clauth never saw alive.
            if seen_alive {
                record_wedged_stop(tree, pid, start);
                if supervisor_spare_should_announce(tree) {
                    logline!(
                        "clauth: {}: sparing: its supervisor pid {pid} is still alive {SUPERVISOR_STOP_TIMEOUT:?} after SIGTERM",
                        tree.display()
                    );
                }
            } else if supervisor_spare_should_announce(tree) {
                logline!(
                    "clauth: {}: sparing: its supervisor pid {pid}'s liveness could not be read for {SUPERVISOR_STOP_TIMEOUT:?} after SIGTERM",
                    tree.display()
                );
            }
            return SupervisorStop::Spare;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// One spare line per tree per process: a tree spared every pass — a
/// reused pid, a supervisor that ignores SIGTERM — logs once, not once per GC
/// tick.
#[cfg(target_os = "linux")]
fn supervisor_spare_should_announce(tree: &Path) -> bool {
    static ANNOUNCED: std::sync::OnceLock<std::sync::Mutex<HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    ANNOUNCED
        .get_or_init(|| std::sync::Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(tree.to_path_buf())
}

/// The cross-process spare dedupe's decision, factored off the platform call so
/// the journal logic unit-tests on every OS: given the already-swept
/// `spared` rows, whether `key` should announce and the rows with a new
/// announcement recorded.
#[cfg(any(not(target_os = "linux"), test))]
fn spared_announce_decision(
    spared: &[SparedAnnounceRow],
    key: &str,
    now_secs: u64,
) -> (bool, Vec<SparedAnnounceRow>) {
    if spared.iter().any(|row| row.tree == key) {
        return (false, spared.to_vec());
    }
    let mut rows = spared.to_vec();
    rows.push(SparedAnnounceRow {
        tree: key.to_string(),
        stamp_secs: now_secs,
    });
    (true, rows)
}

/// Off linux the spare line is the only word a stuck tree ever gets — a tree
/// spared every pass re-reads the unverifiable record (and, on macOS, an argv
/// or format mismatch), so the dedupe is on disk, not per process: one line per
/// tree per [`WEDGED_STOP_SKIP_AFTER`] across the daemon, the TUI and every
/// `clauth start`. A flock or journal that cannot be read announces.
#[cfg(not(target_os = "linux"))]
fn supervisor_spare_should_announce(tree: &Path) -> bool {
    journal_spare_should_announce(tree)
}

/// The cross-process spare announcement: the load -> sweep -> push -> save runs
/// in one state-flock hold, the same shape as [`record_wedged_stop`], so one
/// process cannot overwrite another's flocked in-flight or wedged rows while it
/// records the announcement. Factored off the linux per-process latch so the
/// journal body unit-tests on every OS.
#[cfg(any(not(target_os = "linux"), test))]
fn journal_spare_should_announce(tree: &Path) -> bool {
    let Ok(announce) = with_state_lock(|_held| {
        let mut stops = load_stops_in_flight()?;
        sweep_stale_stops(&mut stops);
        let key = stop_in_flight_key(tree);
        let (announce, rows) =
            spared_announce_decision(&stops.spared, &key, stop_in_flight_now_secs());
        if announce {
            stops.spared = rows;
            let _ = save_stops_in_flight(&stops);
        }
        Ok(announce)
    }) else {
        return true;
    };
    announce
}

fn announce_supervisor_spare(tree: &Path, stale_start: Option<&str>) {
    if supervisor_spare_should_announce(tree) {
        match stale_start {
            Some(stale) => logline!(
                "clauth: {}: sparing: its supervisor's recorded start {stale:?} is stale (the pid still reads as this tree's supervisor)",
                tree.display()
            ),
            None => logline!(
                "clauth: {}: sparing: its supervisor could not be verified as this tree's own",
                tree.display()
            ),
        }
    }
}

/// The stop's wait deadline, shortable by a test so the ignore-SIGTERM arm
/// does not sleep the real [`SUPERVISOR_STOP_TIMEOUT`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn supervisor_stop_deadline() -> Duration {
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    {
        if let Some(d) = SUPERVISOR_STOP_DEADLINE.with(|c| c.get()) {
            return d;
        }
    }
    SUPERVISOR_STOP_TIMEOUT
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
thread_local! {
    static SUPERVISOR_STOP_DEADLINE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) struct SupervisorStopDeadlineGuard(Option<Duration>);

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl SupervisorStopDeadlineGuard {
    pub(crate) fn set(deadline: Duration) -> Self {
        let prev = SUPERVISOR_STOP_DEADLINE.with(|c| c.replace(Some(deadline)));
        Self(prev)
    }

    /// The calling thread's deadline, for a worker it hands a stop to.
    pub(crate) fn current() -> Option<Duration> {
        SUPERVISOR_STOP_DEADLINE.with(std::cell::Cell::get)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl Drop for SupervisorStopDeadlineGuard {
    fn drop(&mut self) {
        SUPERVISOR_STOP_DEADLINE.with(|c| c.set(self.0));
    }
}

/// Whether the tree's own supervisor is (or may be) alive and unverifiable —
/// the acquire consult, run in-flock (pure reads, no signal, no subprocess).
fn supervisor_may_be_alive(tree: &Path) -> bool {
    match supervisor_state(tree) {
        SupervisorState::Unverifiable { .. } => true,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        SupervisorState::Ours { .. } => true,
        SupervisorState::None | SupervisorState::Gone => false,
    }
}

/// A supervisor stop in flight for one runtime tree path. Stamped in the
/// GC/teardown decide hold before the SIGTERM runs and cleared after the
/// removal hold, so an acquire that resolves the same path between the two
/// refuses (the Fake shared tree and the background pair have no other
/// spelling) or re-mints its sid instead of launching a session whose shared
/// supervisor the stop is about to end. Persistent — the stop and the acquire run in different
/// processes. The shape is the macOS keychain in-flight delete's.
///
/// The same journal's `wedged` field records a supervisor that ignored
/// SIGTERM (its pid + start time + when), so no later process re-signals and
/// re-waits that exact supervisor within [`WEDGED_STOP_SKIP_AFTER`] — a
/// wedged `--bg` tree costs every `clauth start` 15 s per pass otherwise.
const STOP_IN_FLIGHT_PATH: &str = "daemon-stops-in-flight.json";

/// A stop's worst-case wall duration (SIGTERM + [`SUPERVISOR_STOP_TIMEOUT`])
/// plus the removal; a row older than this with no sign of its runner is
/// crash-stale.
const STOP_IN_FLIGHT_STALE_AFTER: Duration = Duration::from_secs(30);

/// How long a wedged supervisor is skipped before the next stop attempt.
/// Above [`SUPERVISOR_STOP_TIMEOUT`] by a wide margin, so a supervisor
/// that wedged once is not re-waited by every entry point's GC sweep.
const WEDGED_STOP_SKIP_AFTER: Duration = Duration::from_secs(600);

#[derive(Debug, Default, Serialize, Deserialize)]
struct StopsInFlight {
    stops: Vec<StopInFlightRow>,
    #[serde(default)]
    wedged: Vec<WedgedStopRow>,
    #[serde(default)]
    spared: Vec<SparedAnnounceRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StopInFlightRow {
    tree: String,
    stamp_secs: u64,
}

/// A supervisor that ignored SIGTERM, recorded so the stop is skipped for
/// [`WEDGED_STOP_SKIP_AFTER`]. The `(pid, start)` pair is part of the identity:
/// a NEW supervisor on the same tree (different pid or start) is never skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WedgedStopRow {
    tree: String,
    pid: u32,
    start: String,
    stamp_secs: u64,
}

/// A tree whose unverifiable supervisor already logged its spare line, so no
/// later process re-announces it within [`WEDGED_STOP_SKIP_AFTER`]. Only
/// the non-linux arm consults it; linux keeps the per-process latch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SparedAnnounceRow {
    tree: String,
    stamp_secs: u64,
}

fn stop_in_flight_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(STOP_IN_FLIGHT_PATH))
}

fn load_stops_in_flight() -> Result<StopsInFlight> {
    let path = stop_in_flight_path()?;
    match std::fs::read_to_string(&path) {
        Ok(body) => match serde_json::from_str::<StopsInFlight>(&body) {
            Ok(stops) => Ok(stops),
            // An unparseable journal is logged once and treated as empty,
            // never propagated into every acquire's gate (the write is an
            // atomic rename, but a hand edit or a disk fault can still produce
            // bytes nothing parses). The next record or clear overwrites it.
            Err(_) => {
                announce_corrupt_stop_journal_once(&path);
                Ok(StopsInFlight::default())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StopsInFlight::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// One line per process for an unparseable stop journal, however many
/// acquires consult it.
fn announce_corrupt_stop_journal_once(path: &Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        logline!(
            "clauth: treating an unparseable stop journal {} as empty",
            path.display()
        );
    });
}

fn save_stops_in_flight(stops: &StopsInFlight) -> Result<()> {
    let path = stop_in_flight_path()?;
    // `atomic_write_600` already `mkdir_700`s a missing parent and writes the
    // file 0o600; no `enforce_clauth_perms` walk here — that recursive chmod
    // of every `~/.clauth` entry ran under the state flock on every stop save,
    // and this journal is the only file the save touches.
    let bytes = serde_json::to_vec_pretty(stops)
        .context("failed to serialize the in-flight-stop record")?;
    atomic_write_600(&path, bytes).context("failed to persist the in-flight-stop record")
}

/// Seconds since the UNIX epoch; 0 on a pre-epoch clock (a fresh-looking
/// record refuses rather than reopening the window).
fn stop_in_flight_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn sweep_stale_stops(stops: &mut StopsInFlight) {
    let now = stop_in_flight_now_secs();
    stops.stops.retain(|row| {
        row.stamp_secs
            .saturating_add(STOP_IN_FLIGHT_STALE_AFTER.as_secs())
            > now
    });
    stops.wedged.retain(|row| {
        row.stamp_secs
            .saturating_add(WEDGED_STOP_SKIP_AFTER.as_secs())
            > now
    });
    stops.spared.retain(|row| {
        row.stamp_secs
            .saturating_add(WEDGED_STOP_SKIP_AFTER.as_secs())
            > now
    });
}

fn stop_in_flight_key(tree: &Path) -> String {
    crate::canonical::identity(tree)
        .unwrap_or_else(|_| tree.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Stamp (or re-stamp) the in-flight record for one tree path. The caller
/// holds the state flock: the stamp and the stop's identity decision share one
/// hold, so an acquire serialized after it sees the record and one serialized
/// before it is the supervisor the stop is about to end.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn record_stop_in_flight_locked(tree: &Path) -> Result<()> {
    let key = stop_in_flight_key(tree);
    let mut stops = load_stops_in_flight()?;
    sweep_stale_stops(&mut stops);
    let now = stop_in_flight_now_secs();
    match stops.stops.iter_mut().find(|row| row.tree == key) {
        Some(row) => row.stamp_secs = now,
        None => stops.stops.push(StopInFlightRow {
            tree: key,
            stamp_secs: now,
        }),
    }
    save_stops_in_flight(&stops)
}

/// Clear the in-flight record after the removal hold, in its own flock hold.
/// Best-effort: a leftover row ages out via [`STOP_IN_FLIGHT_STALE_AFTER`].
fn clear_stop_in_flight(tree: &Path) {
    let _ = with_state_lock(|_held| {
        let key = stop_in_flight_key(tree);
        if let Ok(mut stops) = load_stops_in_flight() {
            let before = stops.stops.len();
            stops.stops.retain(|row| row.tree != key);
            if stops.stops.len() != before {
                let _ = save_stops_in_flight(&stops);
            }
        }
        Ok::<_, anyhow::Error>(())
    });
}

/// Whether a stop is in flight for this path. Called under the state flock by
/// acquire; a journal that cannot be READ (permission, a vanished `~/.clauth`)
/// reads as in-flight (fail closed, like the keychain ledger), while an
/// unparseable one reads empty (`load_stops_in_flight`).
fn stop_in_flight(tree: &Path) -> Result<bool> {
    let key = stop_in_flight_key(tree);
    let mut stops = load_stops_in_flight()?;
    sweep_stale_stops(&mut stops);
    Ok(stops.stops.iter().any(|row| row.tree == key))
}

/// Whether `(pid, start)` at `tree` is a known-wedged supervisor: a
/// stop failed for that exact pid + start within [`WEDGED_STOP_SKIP_AFTER`],
/// so re-signalling it would cost another [`SUPERVISOR_STOP_TIMEOUT`] wait
/// with no outcome. A new supervisor (different pid or start) is never
/// skipped. Pure read — no flock, no subprocess, no signal.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wedged_stop_is_fresh(tree: &Path, pid: u32, start: &str) -> bool {
    let Ok(mut stops) = load_stops_in_flight() else {
        return false;
    };
    sweep_stale_stops(&mut stops);
    let key = stop_in_flight_key(tree);
    stops
        .wedged
        .iter()
        .any(|row| row.tree == key && row.pid == pid && row.start == start)
}

/// Record a supervisor that would not stop, in its own flock hold so
/// two processes cannot lose each other's rows. Best-effort: a failed record
/// only costs the next process's stop another wait, never a wrong collect.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn record_wedged_stop(tree: &Path, pid: u32, start: &str) {
    let _ = with_state_lock(|_held| {
        let key = stop_in_flight_key(tree);
        let mut stops = load_stops_in_flight()?;
        sweep_stale_stops(&mut stops);
        let now = stop_in_flight_now_secs();
        match stops.wedged.iter_mut().find(|row| row.tree == key) {
            Some(row) => {
                row.pid = pid;
                row.start = start.to_string();
                row.stamp_secs = now;
            }
            None => stops.wedged.push(WedgedStopRow {
                tree: key,
                pid,
                start: start.to_string(),
                stamp_secs: now,
            }),
        }
        save_stops_in_flight(&stops)
    });
}

/// Test seam: whether the in-flight journal records `pid` as a wedged
/// supervisor, so a test can assert a spare did (or did not) stamp one.
/// Linux-only: its only caller is the linux `/proc`-stat liveness seam test.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn pid_has_wedged_stop(pid: u32) -> bool {
    load_stops_in_flight()
        .map(|stops| stops.wedged.iter().any(|row| row.pid == pid))
        .unwrap_or(false)
}

/// Resolve `name` into a spawnable [`Command`](std::process::Command) so an
/// npm-installed shim launches on Windows too. Rust's bare `Command::new`
/// appends only `.exe` and skips `PATHEXT`, so a `<name>.cmd`/`<name>.bat`
/// (npm global) is invisible and `start`/`delegate` fail with "program not
/// found" even though the user runs the CLI fine by hand. `which_all`
/// enumerates every `PATHEXT` match in `PATH` order; we prefer a native
/// `.exe` over a `.cmd`/`.bat` shim whenever both resolve (the shim adds a
/// cmd.exe hop, and PATH dir order could otherwise surface it first), else
/// take the first match and let std route it through cmd.exe with hardened
/// escaping (post-CVE-2024-24576). Unix keeps the bare lookup.
fn resolve_cli_command(name: &str) -> std::process::Command {
    #[cfg(windows)]
    if let Ok(matches) = which::which_all(name) {
        let all: Vec<std::path::PathBuf> = matches.collect();
        let chosen = all
            .iter()
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")))
            .or_else(|| all.first());
        if let Some(path) = chosen {
            return std::process::Command::new(path);
        }
    }
    std::process::Command::new(name)
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

/// Test-only RAII: force the fake-symlink transport for every subsequent
/// `detect_link_mode` for the guard's lifetime, clearing the process-global
/// override on drop even if the test panics — so a Fake mode can never leak
/// into a concurrent test. Without exposing the private `LinkMode` enum.
/// Serialized against other override users by `HOME_TEST_LOCK`, which the
/// caller already holds via its `HomeSandbox`.
#[cfg(test)]
pub(crate) struct ForcedFakeLinkMode;

#[cfg(test)]
impl ForcedFakeLinkMode {
    pub(crate) fn new() -> Self {
        set_link_mode_override(LinkMode::Fake);
        Self
    }
}

#[cfg(test)]
impl Drop for ForcedFakeLinkMode {
    fn drop(&mut self) {
        clear_link_mode_override();
    }
}

// Test seam: how many times THIS thread has called [`detect_link_mode`], so a
// test can pin that the daemon's tick thread never probes — the probe writes
// shared dotfiles, and a tick-side probe would race every locked `clauth start`
// probe on the same account. Thread-local for the same reason
// `crate::claude_jobs::JOB_TREES_READS` is: a concurrent test thread's probes
// land in its own cell.
#[cfg(test)]
thread_local! {
    static LINK_MODE_PROBE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn link_mode_probe_calls() -> usize {
    LINK_MODE_PROBE_CALLS.with(std::cell::Cell::get)
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
    LINK_MODE_PROBE_CALLS.with(|c| c.set(c.get() + 1));
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
/// three callers turn a zero into retiring a runtime tree, which under
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
///   in `~/.claude/` except `settings.json`, `.credentials.json`, `jobs` and
///   the supervisor's own state (`daemon/`, `daemon.json`, `daemon.lock`,
///   `daemon.status.json`, `daemon.log`, `daemon.scheduled.status.json`) —
///   this includes `projects/`,
///   `todos/`, `statsig/`, `sessions/`, `cache/`, `commands/`, `plugins/`,
///   `tasks/`, `teams/`, `hooks/`, `history.jsonl`, and similar. Claude Code
///   treats these as user-global state so sharing is intentional; per-profile
///   isolation would hide project history and installed commands. The
///   supervisor state is per-config-dir, so sharing it would hand every tree
///   one supervisor's roster.
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
///
/// Top-level `~/.claude/` names a supervisor keys on its own
/// `CLAUDE_CONFIG_DIR`: the `daemon/` roster dir and the lock/status/log
/// siblings beside it, the scheduled worker's `daemon.scheduled.status.json`,
/// and `daemon.json` — the scheduled tasks and configured workers the
/// supervisor READS and fires/starts (2.1.308/2.1.309: "The service that runs
/// background sessions starts the workers listed there", and removing a task
/// says "the daemon will stop firing it on its next reconcile"). A tree's
/// transient supervisor reading a linked home `daemon.json` would fire the
/// home's scheduled tasks and start its workers once per tree. Never
/// linked, copied or mirrored into a tree — a shared tree carrying any of them
/// would read and write the home tree's supervisor state, and a linked
/// `daemon.lock` would point clauth's stop at the home supervisor. Skipping
/// `daemon.json` also means a task added inside a session lives in
/// its own tree's `daemon.json`, not the home's: session-created tasks
/// are tree-local and collected with the tree, the accepted trade for keeping
/// every tree's transient supervisor from firing the home's tasks. One
/// predicate, shared by the build walk and the watchdog mirror.
fn is_daemon_state_name(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            "daemon"
                | "daemon.json"
                | "daemon.lock"
                | "daemon.status.json"
                | "daemon.log"
                | "daemon.scheduled.status.json"
        )
    )
}

fn build_runtime_dir_with_active_env(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    canonical: &Path,
    mode: LinkMode,
    isolation: Isolation,
    stale_env_keys: &[String],
) -> Result<Vec<String>> {
    // Drop any top-level symlink whose `~/.claude/` target has vanished before
    // the re-walk. A prior session's link can dangle once the operator moves the
    // source aside (the reported `runtime/CLAUDE.md` → moved memory case); the
    // walk below only visits entries still in `~/.claude/`, so it would never
    // revisit — and skip — that stale link. Live entries stay; a still-present
    // source gets re-linked by the walk.
    prune_dangling_links(runtime)?;
    let mut store_notes = Vec::new();
    if isolation == Isolation::Shared {
        store_notes = ensure_store_links(claude_home, runtime, mode)?;
    }

    let mut pending: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(claude_home)
        .with_context(|| format!("failed to read {}", claude_home.display()))?
    {
        let entry = entry?;
        let file_name = entry.file_name();
        if file_name == "settings.json"
            || file_name == ".credentials.json"
            // A copy-host tree never gets the job store: a copied store would
            // carry removed jobs back and forth (the mirror skips it too). On a
            // linking host `ensure_store_links` above placed the link, or the
            // entry is unusable and the walk links it as it stands.
            || (mode == LinkMode::Fake && file_name == "jobs")
            || is_daemon_state_name(&file_name)
        {
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

    Ok(store_notes)
}

/// Stores Claude Code writes through its config dir and clauth reads from
/// `~/.claude`, each with the ending of the line a start prints when the
/// store cannot be created.
pub(crate) const SHARED_STORES: &[(&str, &str)] = &[
    (
        "jobs",
        "background jobs stay in its own tree, where `claude agents` outside it cannot see them",
    ),
    (
        "projects",
        "transcripts stay in its own tree and are lost when it ends",
    ),
];

/// Link the stores a shared tree must reach: Claude Code writes `jobs`
/// (background job records) and `projects` (transcripts) through its config
/// dir, and clauth reads them from `~/.claude`. The build's walk only visits
/// what `~/.claude` already holds, so a box that never wrote either offers no
/// store for the walk to link, and the record would land inside the tree,
/// out of sight of `claude agents` run outside it, and a transcript there is
/// never indexed and dies with the tree.
///
/// Shared trees on the linking transport only: a copy-host tree never gets the
/// global job store linked or copied in (the walk and the mirror both skip
/// `jobs`), so a record Claude Code writes there lands in the tree's own
/// `jobs/`, which clauth reads; and an isolated tree reaches
/// nothing in `~/.claude`. Each
/// absent store is created 0700, the owner-only shape every clauth write
/// takes; an existing tree entry is left as it is; a store that is unusable or
/// cannot be created is skipped, so a start the base allowed is never refused
/// by this fix. Each store it cannot create is returned as a note the caller
/// surfaces (the foreground start prints it; the MCP delegate path stays
/// silent).
fn ensure_store_links(claude_home: &Path, runtime: &Path, mode: LinkMode) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    if mode == LinkMode::Fake {
        return Ok(notes);
    }
    for (name, consequence) in SHARED_STORES {
        if let Some(note) = ensure_store_link(claude_home, runtime, name, consequence)? {
            notes.push(note);
        }
    }
    Ok(notes)
}

fn ensure_store_link(
    claude_home: &Path,
    runtime: &Path,
    name: &str,
    consequence: &str,
) -> Result<Option<String>> {
    let global = claude_home.join(name);
    // A regular file, a dangling link or an `ELOOP` loop (`is_dir` swallows
    // the error) is no store: the walk links the tree entry to it as it
    // stands, exactly as the base does.
    if global.symlink_metadata().is_ok() && !global.is_dir() {
        return Ok(None);
    }
    // A start the base allowed must never be refused by this fix: a read-only
    // or full `~/.claude` skips this store and the build goes on.
    if let Err(e) = crate::profile::mkdir_700(&global) {
        return Ok(Some(format!(
            "clauth: could not create {}: {e}; this session's {consequence}",
            global.display()
        )));
    }
    let dst = runtime.join(name);
    if dst.symlink_metadata().is_ok() {
        // A tree entry already in place (a link or a real dir) is left as-is,
        // the walk's existing skip.
        return Ok(None);
    }
    link_entry(&global, &dst)?;
    Ok(None)
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
) -> Result<Vec<String>> {
    let store_notes = build_runtime_dir_with_active_env(
        runtime,
        claude_home,
        profile,
        canonical,
        mode,
        isolation,
        &[],
    )?;
    seed_claude_json(runtime, claude_home)?;
    Ok(store_notes)
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
            // The guard is load-bearing: `Path::exists` swallows every stat
            // error, so a live link over a dropped mount reads as dangling and
            // gets unlinked here — safe only because the re-walk re-links it on
            // the same pass. A dangling survivor is permanent, since the re-walk
            // skips any entry whose `symlink_metadata` succeeds.
            unlink_link(&path, "stale link");
        }
    }
    Ok(())
}

/// Unlink a link itself without traversing it. On unix `remove_file` is the
/// whole story: `unlink` never follows the link. The `remove_dir` fallback is
/// the Windows split, measured twice. Dangling, the merge-base's measurement:
/// `remove_file` clears a dangling file symlink but answers os error 5 on a
/// dangling junction or directory symlink. Live, measured on Windows 11 IoT Ent
/// LTSC 24H2 with rustc 1.96.1, elevated and with
/// `SeCreateSymbolicLinkPrivilege` stripped alike: `remove_file` errors 5 on a
/// live directory symlink and on a live junction, then `remove_dir` returns
/// `Ok`, removes the reparse point, and leaves the target directory and its
/// files intact. On unix `remove_dir_all` on a live directory symlink also
/// unlinks the link and leaves the target intact (Linux, rustc 1.98.0), so
/// unlinking explicitly here is portability rather than data loss.
/// `remove_dir`, never `remove_dir_all`.
fn unlink_link(path: &Path, label: &str) {
    if let Err(file_err) = std::fs::remove_file(path)
        && let Err(dir_err) = std::fs::remove_dir(path)
    {
        logline!(
            "clauth: {label} {} could not be removed ({file_err}; as a dir: {dir_err})",
            path.display()
        );
    }
}

/// The `settings.json` base the runtime overlay builds on for `isolation`: the
/// live `~/.claude/settings.json` under shared isolation, none under isolated
/// (the overlay starts from `{}`). One helper for both the runtime write
/// ([`write_merged_settings`]) and the start demand's derived model read
/// ([`crate::start::member_launch_demand`]), so the two cannot drift on which
/// isolation keeps the global settings.
pub(crate) fn settings_base(claude_home: &Path, isolation: Isolation) -> Option<PathBuf> {
    match isolation {
        Isolation::Shared => Some(claude_home.join("settings.json")),
        Isolation::Isolated => None,
    }
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
    let base = settings_base(claude_home, isolation);
    let merged = build_claude_settings_json(base.as_deref(), profile, stale_env_keys)?;
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
/// across all copies (the winner is the member whose bytes carry the newest
/// Claude Code write). A freshly seeded copy strips the global file's
/// `oauthAccount` (issue #17: a raw copy is born carrying whichever account was
/// active at seed time, wrong for every profile but the active one) so this
/// profile starts identity-less and Claude Code re-derives it from THIS
/// profile's own credentials on first boot; that boot (or the next OAuth login)
/// writes the correct identity, which the syncer then preserves as this copy's
/// own per-profile field.
///
/// Seeds from the global file when this profile has no real copy yet, or
/// migrates the old shared symlink (pre-per-profile behavior) to a copy. The
/// copy is written at owner-only mode (the seed carries the account's
/// `oauthAccount` billing/identity caches), replacing a symlink in one step —
/// no window where a sibling session sees the file missing. Existing real
/// copies keep their own identity and synced shared fields.
///
/// The write is recorded under [`crate::claude_json::JsonSyncLock`] with the
/// carried rank of the bytes it was built from (home's own rank via
/// [`crate::jsonsync::carried_rank_of`]): the mtime of the Claude Code write
/// those bytes carry, so the seed echoes that write's rank. The copy's mtime
/// is stamped at the floor stamp ([`crate::jsonsync::FLOOR_STAMP_SECS`], below
/// every Claude Code write) — the same stamp the unrecorded seed uses — so a
/// recorded seed whose record write fails (fail-open, logged) ranks below every
/// Claude Code write instead of reverting a sibling's newer write on its own
/// fresh mtime; the record entry is taken from the temp handle after the stamp,
/// so it still matches. The seed must run OUTSIDE the
/// state flock (`JsonSync` (480) ranks OUTSIDE `State` (500), taken with no
/// state flock held), which is why the caller seeds after the flock release.
/// A seed that cannot take the lock within
/// [`crate::claude_json::SEED_SYNC_LOCK_BUDGET`] (or resolve the record path)
/// still seeds — a session must start — and stamps the copy's mtime at the
/// floor stamp likewise, so an unrecorded seed never outranks a sibling's newer
/// write: the copy holds only home's bytes, which home keeps at its own rank,
/// so it never needs to win a pass.
fn seed_claude_json(runtime: &Path, claude_home: &Path) -> Result<()> {
    let Some(home) = claude_home.parent() else {
        return Ok(());
    };
    let global = home.join(".claude.json");
    let dst = runtime.join(".claude.json");
    let is_symlink = dst
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink());
    if !(is_symlink || !dst.exists()) {
        return Ok(());
    }
    let Some(record) = crate::claude_json::written_record_path() else {
        // Residual: no record path — seed unrecorded, stamped at the floor.
        return seed_unrecorded(&global, &dst);
    };
    #[cfg(test)]
    run_seed_pre_lock_hook();
    let Ok(_lock) = crate::claude_json::JsonSyncLock::acquire_with_timeout(
        crate::claude_json::SEED_SYNC_LOCK_BUDGET,
    ) else {
        // Residual: the lock could not be taken within the healthy budget —
        // seed unrecorded, stamped at the floor.
        return seed_unrecorded(&global, &dst);
    };
    // Stat and read home under the lock: the bytes and their carried rank are
    // home's as they stand under the lock, so a merge holding the lock cannot
    // replace home's record entry between the read and the rank. A missing,
    // unreadable or unstatable home has nothing to seed.
    let Some((home_mtime, home_size, home_bytes)) = read_home_for_seed(&global) else {
        return Ok(());
    };
    let bytes = strip_oauth_account_on_seed(&home_bytes);
    let carried_rank =
        crate::jsonsync::carried_rank_of(&record, &global, home_mtime, home_size, &home_bytes);
    crate::jsonsync::write_copy_and_record(
        Some(&record),
        &dst,
        &bytes,
        carried_rank,
        true,
        Some(crate::jsonsync::floor_stamp()),
    )
    .with_context(|| format!("failed to seed {}", dst.display()))
}

/// Test-only seam right before the seed takes the sync lock, where a merge
/// rewriting home lands deterministically. Armed only while the arming test
/// holds a `HomeSandbox`, so `HOME_TEST_LOCK` serializes it.
#[cfg(test)]
static SEED_PRE_LOCK_HOOK: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> =
    std::sync::Mutex::new(None);

/// Arm or clear [`SEED_PRE_LOCK_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_seed_pre_lock_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *SEED_PRE_LOCK_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Run the armed [`SEED_PRE_LOCK_HOOK`] outside its slot lock.
#[cfg(test)]
fn run_seed_pre_lock_hook() {
    let hook = SEED_PRE_LOCK_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Read home's `.claude.json` stat + size + bytes together, so the carried rank
/// matches the bytes the seed was built from. `None` for a missing, unreadable
/// or unstatable home.
fn read_home_for_seed(global: &Path) -> Option<(SystemTime, u64, Vec<u8>)> {
    let meta = std::fs::metadata(global).ok()?;
    let mtime = meta.modified().ok()?;
    let bytes = std::fs::read(global).ok()?;
    Some((mtime, meta.len(), bytes))
}

/// Seed the runtime copy unrecorded when the record path is absent or the sync
/// lock could not be taken within the healthy budget (a session must start):
/// read home best-effort and write the stripped copy stamped at the floor
/// stamp, below every Claude Code write — exactly as the wire's created home.
/// The copy holds only home's bytes, which home keeps at its own rank, so it
/// never needs to win a pass; the floor stamp ranks it below every Claude Code
/// write without reading home's rank without the lock.
fn seed_unrecorded(global: &Path, dst: &Path) -> Result<()> {
    let Some((_mtime, _size, home_bytes)) = read_home_for_seed(global) else {
        return Ok(()); // nothing to seed
    };
    let bytes = strip_oauth_account_on_seed(&home_bytes);
    crate::jsonsync::write_copy_and_record(
        None,
        dst,
        &bytes,
        (0, 0),
        true,
        Some(crate::jsonsync::floor_stamp()),
    )
    .with_context(|| format!("failed to seed {}", dst.display()))
}

/// Remove `oauthAccount` from freshly seeded `.claude.json` bytes. A no-op
/// (returns the bytes unchanged) when the key is already absent or the source
/// doesn't parse as a JSON object, so the common case stays a plain byte copy.
fn strip_oauth_account_on_seed(bytes: &[u8]) -> Vec<u8> {
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_slice::<serde_json::Value>(bytes)
    else {
        return bytes.to_vec();
    };
    if obj.remove("oauthAccount").is_none() {
        return bytes.to_vec();
    }
    serde_json::to_vec_pretty(&serde_json::Value::Object(obj)).unwrap_or_else(|_| bytes.to_vec())
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

/// One per-session Keychain item write or sign-out, past the caller's carry:
/// `authorize` persists the ownership row (a state-flock hold), and only once
/// it returns does `leg` run under one [`crate::lock::SharedSubprocessBudget`]
/// of `budget` ([`crate::lock::with_keychain_budget`]), so a peer's flock hold
/// spends none of the window `leg`'s `security` calls share. The swap's item
/// write and sign-out, the convergence's sign-out, the seed's write and
/// sign-out and the seed's retry all run through here, so the order has one
/// home a linux test drives. The carry stays at each caller: the swap picks
/// its leg off the incoming store after it, and each site logs a failed carry
/// in its own words. A source scan (`no_item_leg_folds_in_a_carry_or_a_flock`)
/// reds a call here that names the carry, `with_state_lock` or the row
/// retouch; a carry or a flock reached through any other helper it does not
/// see. The
/// callers are macOS-only and `keychain::enabled()` is false under
/// `cfg(test)`, so no linux test runs them.
///
/// Entered with no budget armed: [`crate::lock::SharedSubprocessBudget::arm`]
/// adopts an outer one, whose window a flock wait above this call (the carry's
/// store write) may already have spent.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the production callers are the macOS swap, convergence and seed Keychain legs; a linux test drives this function directly"
    )
)]
fn owned_keychain_leg<T>(
    budget: Duration,
    authorize: impl FnOnce() -> Result<namespaced_keychain_ledger::OwnedKeychainWrite>,
    leg: impl FnOnce(&namespaced_keychain_ledger::OwnedKeychainWrite) -> Result<T>,
) -> Result<T> {
    debug_assert!(
        crate::lock::armed_budget_remaining().is_none(),
        "a per-session Keychain item leg was entered under an armed budget (a budget \
         armed above it, or a state-flock hold, which arms one): the waits in between \
         may have spent its window; arm nothing and hold no state flock above \
         `owned_keychain_leg`"
    );
    let owned = authorize()?;
    crate::lock::with_keychain_budget(budget, || leg(&owned))
}

/// One live session's runtime tree, with what adopting its per-session item
/// would record: the member it is on and its session id.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point"
    )
)]
pub(crate) struct LiveRuntime {
    pub(crate) runtime: PathBuf,
    pub(crate) profile: ProfileName,
    pub(crate) session: String,
    pub(crate) background: bool,
}

/// The runtime trees of every live `clauth start` session (and background
/// pair) of the Claude harness: the config dirs whose per-session Keychain
/// items a daemon-side writer may touch. A row is live by the same probe the
/// tally uses (`current_member` first, then `start_profile`), which is also
/// the member recorded; its tree must exist. Items of sessions that are not
/// live are the orphan GC's, never this.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point"
    )
)]
pub(crate) fn live_session_runtime_dirs() -> Vec<LiveRuntime> {
    let mut out: Vec<LiveRuntime> = Vec::new();
    for row in crate::live_sessions::list() {
        if row.harness != crate::harness::Harness::Claude || !is_session_id(&row.session_id) {
            continue;
        }
        let member = row.current_member.as_deref().unwrap_or(&row.start_profile);
        let probe = ProfileName::from(member);
        if !session_row_is_live(&probe, row.isolated, &row.session_id) {
            continue;
        }
        let start = ProfileName::from(row.start_profile.as_str());
        let tree = if row.background {
            background_runtime_tree(&start).ok()
        } else {
            session_runtime_trees(&start, row.isolated, &row.session_id)
                .ok()
                .and_then(|trees| trees.into_iter().find(|tree| tree.is_dir()))
        };
        if let Some(tree) = tree.filter(|tree| tree.is_dir())
            && !out.iter().any(|live| live.runtime == tree)
        {
            out.push(LiveRuntime {
                runtime: tree,
                profile: probe,
                session: row.session_id.clone(),
                background: row.background,
            });
        }
    }
    out
}

/// Whether a daemon-side writer may adopt the unowned per-session item of
/// `runtime`: a per-session tree whose dir name carries exactly the live
/// session's id `sid`, or, for a background row, the account's fixed
/// `runtime-bg` tree, which every job on that account shares and which the
/// orphan GC collects like any tree. Anything else, a foreign
/// `CLAUDE_CONFIG_DIR` among them (#81), is never adopted. PURE.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point"
    )
)]
pub(crate) fn session_item_may_adopt(runtime: &Path, sid: &str, background: bool) -> bool {
    let name = runtime.file_name().and_then(|n| n.to_str());
    if !is_session_id(sid) {
        return false;
    }
    if background {
        return name.is_some_and(|n| n == background_dir_names().0);
    }
    name.and_then(sid_of_runtime_dir_name)
        .is_some_and(|own| own == sid)
}

/// The live sessions a stamp of `member`'s sidecar concerns: those whose
/// member is `member`. PURE.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller is the macOS copied-bearer re-point"
    )
)]
pub(crate) fn sessions_on_member(live: Vec<LiveRuntime>, member: &ProfileName) -> Vec<LiveRuntime> {
    live.into_iter().filter(|l| &l.profile == member).collect()
}

/// Rolling bearers a stamp revoked, waiting for [`repoint_copied_bearers`]:
/// the member, the revoked bearer, when the next attempt is due with the
/// backoff it now waits ([`next_item_retry_gap`]), and whether a failure of
/// it is already in the log. A new pair is due at once with a zero gap, so its
/// first attempt may run as soon as a drain does.
#[derive(Debug)]
struct QueuedRevokedBearer {
    member: ProfileName,
    revoked: String,
    due: Instant,
    gap: Duration,
    logged: bool,
}

/// The re-points above. A process-wide queue, so a stamp made inside a
/// state-flock hold or an armed subprocess budget is re-pointed by the next
/// drain that runs outside both. Per thread under test, so sibling tests do
/// not see each other's stamps.
#[cfg(not(test))]
static REVOKED_BEARERS: std::sync::Mutex<Vec<QueuedRevokedBearer>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
thread_local! {
    static REVOKED_BEARERS: std::cell::RefCell<Vec<QueuedRevokedBearer>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn with_revoked_bearers<R>(f: impl FnOnce(&mut Vec<QueuedRevokedBearer>) -> R) -> R {
    #[cfg(not(test))]
    {
        let mut queue = REVOKED_BEARERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut queue)
    }
    #[cfg(test)]
    {
        REVOKED_BEARERS.with(|q| f(&mut q.borrow_mut()))
    }
}

/// Queue `entry` for [`repoint_copied_bearers`], keeping the timer of a pair
/// already queued: a stamp that revokes the same bearer again asks for the
/// same re-point, and starting it over could only postpone a failing pair.
fn queue_revoked_bearer(entry: QueuedRevokedBearer) {
    with_revoked_bearers(|q| {
        if !q
            .iter()
            .any(|e| e.member == entry.member && e.revoked == entry.revoked)
        {
            q.push(entry);
        }
    });
}

/// Record that a stamp of `member`'s sidecar revoked `revoked`
/// ([`crate::claude::revoked_by_stamp`]). Only macOS keeps per-session items,
/// so elsewhere nothing is queued outside tests.
pub(crate) fn note_revoked_rolling_bearer(member: &ProfileName, revoked: String) {
    if !(cfg!(target_os = "macos") || cfg!(test)) {
        return;
    }
    queue_revoked_bearer(QueuedRevokedBearer {
        member: member.clone(),
        revoked,
        due: Instant::now(),
        gap: Duration::ZERO,
        logged: false,
    });
}

/// Take the queued `(member, revoked bearer)` pairs, in queue order. For
/// tests: [`drain_revoked_bearers_with`] takes only the pairs due at its
/// `now` and leaves the rest queued.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "only the tests take the whole queue back")
)]
pub(crate) fn take_revoked_bearers() -> Vec<(ProfileName, String)> {
    with_revoked_bearers(|q| {
        std::mem::take(q)
            .into_iter()
            .map(|e| (e.member, e.revoked))
            .collect()
    })
}

/// How long one drain may spend on Keychain legs, across every pair. A pair
/// the deadline cuts short is queued again for the next drain.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only reader is the macOS copied-bearer re-point"
    )
)]
const REPOINT_DRAIN_BUDGET: Duration = Duration::from_secs(20);

/// Re-point the per-session Keychain items that hold a copy of a rolling
/// bearer a stamp just revoked (see `wiki/Auto-Switch.md`). Called right
/// after the stamp's callers release their locks: a drain inside a state-flock
/// hold or an armed subprocess budget does nothing and leaves the queue for
/// the next one, so no Keychain leg ever runs under either. Each pair due now
/// runs through [`drain_revoked_bearers_with`], which backs a failed one off
/// and leaves one the deadline cut short due, so the next drain retries it.
/// macOS only; a no-op elsewhere and under `cfg(test)`, where
/// `keychain::enabled()` is false.
pub(crate) fn repoint_copied_bearers() {
    #[cfg(target_os = "macos")]
    {
        let now = Instant::now();
        if !crate::keychain::enabled()
            || crate::lock::state_lock_held()
            || crate::lock::armed_budget_remaining().is_some()
            || with_revoked_bearers(|q| q.iter().all(|e| e.due > now))
        {
            return;
        }
        let deadline = now + REPOINT_DRAIN_BUDGET;
        let live = live_session_runtime_dirs();
        let owned = namespaced_keychain_ledger::owned_services().unwrap_or_default();
        drain_revoked_bearers_with(
            &live,
            now,
            deadline,
            |member, revoked, live, logged: &mut bool| {
                let Ok(sidecar) =
                    crate::profile::profile_dir(member).map(|d| d.join("session-token.json"))
                else {
                    return Ok(ItemRepoint::NotHeld);
                };
                let Ok(service) = crate::keychain::keychain_service_for_config_dir(&live.runtime)
                else {
                    return Ok(ItemRepoint::NotHeld);
                };
                let is_owned = owned.contains(&service);
                if !is_owned
                    && !session_item_may_adopt(&live.runtime, &live.session, live.background)
                {
                    return Ok(ItemRepoint::NotHeld);
                }
                let outcome = owned_keychain_leg(
                    KEYCHAIN_MIRROR_BUDGET.min(deadline.saturating_duration_since(Instant::now())),
                    || {
                        if is_owned {
                            namespaced_keychain_ledger::authorize_owned_write(&live.runtime)
                        } else {
                            namespaced_keychain_ledger::authorize_live_write(
                                &live.runtime,
                                &live.profile,
                                &live.session,
                                live.background,
                            )
                        }
                    },
                    |owned| {
                        crate::keychain::repoint_config_dir_item_bearer(
                            &live.runtime,
                            owned,
                            revoked,
                            &sidecar,
                            member,
                        )
                    },
                );
                match outcome {
                    Ok(ItemRepoint::Written) => {
                        logline!(
                            "clauth: session {} held a copy of '{member}' rolling bearer that its last \
                         re-stamp replaced; its per-session Keychain item now holds the current one",
                            live.session
                        );
                        Ok(ItemRepoint::Written)
                    }
                    Ok(outcome) => Ok(outcome),
                    Err(e) => {
                        if !*logged && !crate::keychain::failed_on_locked_keychain(&e) {
                            logline!(
                                "clauth: could not re-point session {}'s per-session Keychain item at \
                             '{member}' current rolling bearer: {e:#}",
                                live.session
                            );
                            *logged = true;
                        }
                        if e.downcast_ref::<crate::keychain::ItemReadFailed>()
                            .is_some()
                        {
                            Ok(ItemRepoint::NotHeld)
                        } else {
                            Err(e)
                        }
                    }
                }
            },
        );
    }
}

/// Drain the queued re-points due at `now`, running `leg` once for each of a
/// pair's member's live sessions. Its `&mut bool` is the pair's "a failure is
/// already in the log" flag, which persists across retries: a leg logs a
/// failure only while it is unset and sets it when it does, so a pair logs one
/// failure however long it retries.
///
/// A pair settles once `leg` has settled each of its member's sessions:
/// [`ItemRepoint::Written`] (the item now holds the current bearer) and
/// [`ItemRepoint::NotHeld`] (nothing there to re-point, ever) each settle
/// one. A pair with a leg that returned `Err` or
/// [`ItemRepoint::NotYet`] is queued again after [`next_item_retry_gap`],
/// taking that backoff even when the deadline also cut it short, and its
/// other sessions still run. A pair the deadline cut short (its check,
/// `Instant::now() >= deadline`, runs before each leg) with no failed leg is
/// queued again with its timer unchanged, so it stays due. A pair whose
/// member has no live session settles here: no leg would ever run for it.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller outside tests is the macOS copied-bearer re-point"
    )
)]
pub(crate) fn drain_revoked_bearers_with(
    live: &[LiveRuntime],
    now: Instant,
    deadline: Instant,
    mut leg: impl FnMut(&ProfileName, &str, &LiveRuntime, &mut bool) -> Result<ItemRepoint>,
) {
    let due = with_revoked_bearers(|q| {
        let (due, later) = std::mem::take(q).into_iter().partition(|e| e.due <= now);
        *q = later;
        due
    });
    for mut entry in due {
        let mut failed = false;
        let mut cut_short = false;
        for target in sessions_on_member(live.to_vec(), &entry.member) {
            if Instant::now() >= deadline {
                cut_short = true;
                break;
            }
            match leg(&entry.member, &entry.revoked, &target, &mut entry.logged) {
                Ok(ItemRepoint::Written | ItemRepoint::NotHeld) => {}
                Ok(ItemRepoint::NotYet) | Err(_) => failed = true,
            }
        }
        if failed {
            let gap = next_item_retry_gap(entry.gap);
            queue_revoked_bearer(QueuedRevokedBearer {
                due: now + gap,
                gap,
                ..entry
            });
        } else if cut_short {
            queue_revoked_bearer(entry);
        }
    }
}

/// What one session's re-point leg found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only caller outside tests is the macOS copied-bearer re-point"
    )
)]
pub(crate) enum ItemRepoint {
    /// The item now holds the current rolling bearer.
    Written,
    /// Nothing this leg can ever re-point: the item no longer holds the
    /// revoked bearer or none exists, or the session's item is one clauth
    /// may not write (no ledger row and not adoptable, no derivable service
    /// or profile dir) or cannot read (macOS `keychain::ItemReadFailed`).
    /// Settled for good.
    NotHeld,
    /// The item still holds the revoked bearer, but the sidecar cannot
    /// replace it yet.
    NotYet,
}

/// [`repoint_copied_bearers`] on a detached thread, for the daemon's run
/// loop: only when a pair is due, and never two at once.
pub(crate) fn repoint_copied_bearers_detached() {
    use std::sync::atomic::{AtomicBool, Ordering as O};
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    let now = Instant::now();
    if cfg!(test)
        || with_revoked_bearers(|q| q.iter().all(|e| e.due > now))
        || IN_FLIGHT.swap(true, O::AcqRel)
    {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("clauth-bearer-repoint".into())
        .spawn(|| {
            repoint_copied_bearers();
            IN_FLIGHT.store(false, O::Release);
        });
    if spawned.is_err() {
        IN_FLIGHT.store(false, O::Release);
    }
}

/// Which Keychain arm a session start takes, derived from the install source
/// alone. PURE so the arm selection is pinned on every platform: the seeding
/// itself is macOS-only and unreachable by `cfg(test)` (`keychain::enabled()`
/// is false there), the same split the mirror sites record.
///
/// - absent install source → [`SessionSeedArm::SignOut`]: the carry would
///   resurrect a departed OAuth store out of a stale item (an endpoint
///   recapture's leftover), so the session's item must stop serving whatever
///   login it holds.
/// - unparseable store → [`SessionSeedArm::Carry`]: a torn read is not
///   evidence of refreshlessness, and the legs that follow fail loudly on
///   the bytes rather than silently skipping (`checked_store_at` refuses a
///   non-login; the carry reads a torn store as holding no expiry, which the
///   item's login beats and adopts).
/// - refreshless store (no refresh token — a rolling sidecar, a static
///   setup-token mint) → [`SessionSeedArm::Skip`]: CC never migrates those,
///   so the file layer must stay authoritative for the session's whole life;
///   writing the item anyway would strand the session on a snapshot the
///   re-stamp leg can no longer reach, because CC does not go back to the
///   file once the item exists.
/// - otherwise → [`SessionSeedArm::Carry`]: carry the item's fresher pair
///   back into the store, then write the item from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumer is the macOS session-start Keychain seed; the arm selection is pinned on every platform"
    )
)]
enum SessionSeedArm {
    SignOut,
    Skip,
    Carry,
}

/// See [`SessionSeedArm`]. `store` is the install source's parsed contents
/// when it both exists and parses; `None` covers absent and unparseable
/// alike, which take different arms only on the `exists` bit the caller
/// already holds.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumer is the macOS session-start Keychain seed; the arm selection is pinned on every platform"
    )
)]
fn session_seed_arm(
    canonical_exists: bool,
    store: Option<&crate::profile::ClaudeCredentials>,
) -> SessionSeedArm {
    if !canonical_exists {
        return SessionSeedArm::SignOut;
    }
    match store {
        None => SessionSeedArm::Carry,
        Some(creds) if creds.refresh_token().is_none() => SessionSeedArm::Skip,
        Some(_) => SessionSeedArm::Carry,
    }
}

/// Which Keychain arm the swap executor's item-write takes for the member being
/// swapped ONTO, derived from that member's install source alone. PURE so the
/// arm selection is pinned on every platform: the write itself is macOS-only
/// and unreachable by `cfg(test)` (`keychain::enabled()` is false there), the
/// same split [`SessionSeedArm`] records — this is the swap-site twin of the
/// start site's refreshless arm.
///
/// - refreshless store (no refresh token — a rolling sidecar, a static
///   setup-token mint) → [`SwapItemArm::SignOut`]: CC never migrates those
///   into an item, so the file layer must stay authoritative for the re-stamp
///   leg to reach the session. The action is a sign-out rather than the start
///   site's bare skip because the swap's item already EXISTS and holds the
///   OUTGOING member's login — merely skipping the write would leave the
///   session authenticating as the member the chain just moved it off of (CC
///   resolves the Keychain first), an inert swap. Nor is a strip-and-rewrite
///   enough: an item left holding only `mcpOAuth` still wins over the file and
///   reads as "Not logged in". The arm hands the item over instead
///   (`keychain::keychain_hand_off_for_config_dir`): its MCP logins move into
///   the incoming store and the item is deleted, which is what drops CC back
///   onto the file with those logins kept.
/// - otherwise → [`SwapItemArm::Install`]: install the incoming store into the
///   item after the carry-back. An unparseable read lands here too: a torn
///   read is not evidence of refreshlessness, and the install leg that follows
///   fails loudly on the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumer is the macOS swap executor's Keychain item-write; the arm selection is pinned on every platform"
    )
)]
enum SwapItemArm {
    Install,
    SignOut,
}

/// See [`SwapItemArm`]. `store` is the incoming member's install source parsed;
/// `None` (absent or unparseable) takes [`SwapItemArm::Install`], never a skip.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumer is the macOS swap executor's Keychain item-write; the arm selection is pinned on every platform"
    )
)]
fn swap_item_arm(store: Option<&crate::profile::ClaudeCredentials>) -> SwapItemArm {
    match store {
        Some(creds) if creds.refresh_token().is_none() => SwapItemArm::SignOut,
        _ => SwapItemArm::Install,
    }
}

/// The Keychain arm for the same-member convergence, pinned pure like
/// [`swap_item_arm`] while the macOS leg that executes it is unreachable by
/// `cfg(test)`. The convergence admits only a refreshless selected source
/// ([`converge_transition_holds`]), so the arm is [`SwapItemArm::SignOut`]
/// for every store the transition can select — the bearer is never installed
/// into the item, which would strand the session on a snapshot the re-stamp
/// leg can no longer reach. Delegates to [`swap_item_arm`] so the one
/// refreshless→SignOut rule has one home.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS convergence leg; the arm choice is pinned on every platform"
    )
)]
fn converge_item_arm(store: Option<&crate::profile::ClaudeCredentials>) -> SwapItemArm {
    swap_item_arm(store)
}

/// What a convergence Keychain leg does with a failure's classified exit.
/// PURE so the routing is pinned on every platform while the legs themselves
/// only a Mac exercises: the locked-keychain transient (the one measured
/// shape a locked keychain hands a read back) stops silently — it is the
/// steady state the next poll clears once the keychain unlocks, and a line
/// per tick for its whole duration is the noise the seed's disposition
/// pattern exists to avoid. Every other class announces through the cell's
/// once-per-(member, reason) memo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production callers are the macOS convergence legs; the routing is pinned on every platform"
    )
)]
enum ConvergeLegDisposition {
    Silent,
    Announce,
}

/// See [`ConvergeLegDisposition`].
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production callers are the macOS convergence legs; the routing is pinned on every platform"
    )
)]
fn converge_leg_disposition(class: crate::claude::SecurityExitClass) -> ConvergeLegDisposition {
    match class {
        crate::claude::SecurityExitClass::InteractionNotAllowed => ConvergeLegDisposition::Silent,
        crate::claude::SecurityExitClass::ItemNotFound
        | crate::claude::SecurityExitClass::Unclassified => ConvergeLegDisposition::Announce,
    }
}

/// What the session-start Keychain seed does with a leg that failed, keyed on
/// the `security` exit classification: the one measured transient (a locked
/// keychain, [`crate::claude::SecurityExitClass::InteractionNotAllowed`])
/// clears the moment the keychain unlocks, so its degrade arms a retry on this
/// session's watchdog credential tick — the retry is the recomputation, no
/// queue and no timer of its own — while every other failure keeps the pre-fix
/// loud degrade. The swap's item legs route their failures through the same
/// rule ([`keychain_leg_retry_clears`]), so a locked keychain mid-session
/// retries on the session's swap poll. PURE so the disposition is pinned on every
/// platform; the seed, the swap legs and the retries that consult it are
/// macOS-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumers are the macOS seed, the swap's item legs and their watchdog retries; the disposition is pinned on every platform"
    )
)]
enum SeedDegradeDisposition {
    /// Re-run the seed's carry-then-write on the watchdog's credential ticks
    /// while the store still matches the seed's target.
    RetryOnTick,
    /// The pre-fix behavior: log the consequence, degrade to the file layer,
    /// and leave the re-run to the next start on the same tree.
    LogAndDegrade,
}

/// See [`SeedDegradeDisposition`].
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumers are the macOS seed, the swap's item legs and their watchdog retries; the disposition is pinned on every platform"
    )
)]
fn seed_degrade_disposition(class: crate::claude::SecurityExitClass) -> SeedDegradeDisposition {
    match class {
        crate::claude::SecurityExitClass::InteractionNotAllowed => {
            SeedDegradeDisposition::RetryOnTick
        }
        crate::claude::SecurityExitClass::ItemNotFound
        | crate::claude::SecurityExitClass::Unclassified => SeedDegradeDisposition::LogAndDegrade,
    }
}

/// Whether a failed seed leg was refused over a sweep's still-in-flight
/// delete: the consult's typed refusal, mapped to the retry arm of
/// [`SeedDegradeDisposition`] — the record clears when the delete lands or
/// fails, and a crashed delete's record is swept once its stamped child is
/// dead and the age bound passes, so the watchdog-tick retry converges. PURE
/// so the mapping is pinned on every platform; the classified half of the
/// disposition is macOS-only.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumers are the macOS seed, the swap's item legs and their watchdog retries; the mapping is pinned on every platform"
    )
)]
fn delete_in_flight_disposition(e: &anyhow::Error) -> Option<SeedDegradeDisposition> {
    e.downcast_ref::<namespaced_keychain_ledger::DeleteInFlight>()
        .is_some()
        .then_some(SeedDegradeDisposition::RetryOnTick)
}

/// What a failed per-session Keychain leg reduces to for the retry decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "built only from the macOS Keychain legs' failures; the retry routing is pinned on every platform"
    )
)]
enum KeychainLegFault {
    /// The hand-off found the keychain locked and left the item untouched.
    SkippedLocked,
    /// A sweep's delete of the item was still in flight.
    DeleteInFlight,
    /// Any other failure, by its `security` exit class.
    Exit(crate::claude::SecurityExitClass),
}

const KEYCHAIN_UNLOCK_CLEARS: &str = "so it lands once the keychain unlocks";
const SWEEP_DELETE_CLEARS: &str =
    "so it lands once the sweep's delete completes and its in-flight record clears";

/// The clause a retrying degrade line ends on, naming what clears `fault`,
/// when the leg retries on the watchdog ([`SeedDegradeDisposition::RetryOnTick`]);
/// `None` when it does not, and only a later start or swap re-runs it. PURE
/// so the routing is pinned on every platform; the legs whose failures feed
/// it are macOS-only.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only consumers are the macOS seed, the swap's item legs and their retries; the routing is pinned on every platform"
    )
)]
fn keychain_leg_retry_clears(fault: KeychainLegFault) -> Option<&'static str> {
    match fault {
        KeychainLegFault::SkippedLocked => Some(KEYCHAIN_UNLOCK_CLEARS),
        KeychainLegFault::DeleteInFlight => Some(SWEEP_DELETE_CLEARS),
        KeychainLegFault::Exit(class) => (seed_degrade_disposition(class)
            == SeedDegradeDisposition::RetryOnTick)
            .then_some(KEYCHAIN_UNLOCK_CLEARS),
    }
}

/// [`keychain_leg_retry_clears`] for a leg's error.
#[cfg(target_os = "macos")]
fn retry_on_tick_clears(e: &anyhow::Error) -> Option<&'static str> {
    keychain_leg_retry_clears(if delete_in_flight_disposition(e).is_some() {
        KeychainLegFault::DeleteInFlight
    } else {
        KeychainLegFault::Exit(crate::keychain::classified_exit(e))
    })
}

/// Why a swap's item legs left the item unlanded.
#[cfg(target_os = "macos")]
enum SwapItemFailure {
    /// The carry back into the outgoing store failed, so neither leg wrote.
    Carry(anyhow::Error),
    /// The install of the incoming store into the item failed.
    Write(anyhow::Error),
    /// The hand-off of the item to a refreshless store failed.
    SignOut(anyhow::Error),
    /// The hand-off found the keychain locked and left the item untouched.
    SkippedLocked,
}

#[cfg(target_os = "macos")]
impl SwapItemFailure {
    /// The retry clause when this failure clears on its own (the seed's rule,
    /// plus the locked-keychain skip), `None` when only a later swap re-runs
    /// the legs.
    fn retry_clears(&self) -> Option<&'static str> {
        match self {
            Self::Carry(e) | Self::Write(e) | Self::SignOut(e) => retry_on_tick_clears(e),
            Self::SkippedLocked => keychain_leg_retry_clears(KeychainLegFault::SkippedLocked),
        }
    }

    /// What failed and what it left the session reading, then the legs a
    /// re-run repeats as the no-retry line names them and as the retry line
    /// names them.
    fn describe(&self) -> (String, &'static str, &'static str) {
        match self {
            Self::Carry(e) => (
                format!(
                    "carrying the previous member's Keychain pair back into its store failed: \
                     {e:#}. The per-session Keychain item was left untouched, so the session \
                     keeps authenticating as its previous member"
                ),
                "both legs",
                "both legs",
            ),
            Self::Write(e) => (
                format!(
                    "writing its per-session Keychain item failed: {e:#}. The session keeps \
                     authenticating as its previous member"
                ),
                "the write",
                "the write",
            ),
            Self::SignOut(e) => (
                format!(
                    "signing its per-session Keychain item out failed: {e:#}. The session keeps \
                     authenticating as its previous member, so the file layer this swap moved is \
                     not what its Claude Code reads"
                ),
                "it",
                "the sign-out",
            ),
            Self::SkippedLocked => (
                "signing its per-session Keychain item out was skipped: the keychain is locked, \
                 so the item was left untouched and the session keeps authenticating as its \
                 previous member. Rotations stay refused for it"
                    .to_string(),
                "the sign-out",
                "the sign-out",
            ),
        }
    }
}

/// macOS: seed the NAMESPACED Keychain item a session's Claude Code reads
/// (it sets `CLAUDE_CONFIG_DIR`, and CC resolves the Keychain before any
/// file), from the same install source the runtime tree was just built
/// from — the Keychain half of [`reconcile_credentials`]' file half.
///
/// Without a pre-written item, a session on a refreshable login falls
/// through to the runtime file, migrates into the item on its first token
/// write and DELETES that file: the store never advances, its refresh token
/// goes stale, and the next clauth-side rotation dies into quarantine
/// (reproduced end-to-end on-device 2026-09-11). A
/// pre-written item means CC reads clauth's pair from its first request and
/// the runtime file keeps feeding the drain.
///
/// REFRESHLESS install sources skip both legs: a rolling sidecar or a
/// static setup-token mint never refreshes, so CC never migrates it and the
/// file layer stays authoritative for the session's whole life — writing
/// the item anyway would strand the session on a snapshot the re-stamp leg
/// can no longer reach, because CC does not go back to the file once the
/// item exists.
///
/// An ABSENT install source signs the item out instead of carrying: the
/// carry would resurrect a departed OAuth store out of a stale item (an
/// endpoint recapture's leftover), and the session's CC must not keep
/// serving that login — the same split the bare-item mirror makes
/// (`keychain_mirror_source`'s `AbsentSource::SignOut`).
///
/// CARRY, then WRITE — the same pair and order as `swap_to`: the item may
/// already hold a login (a live sibling on the shared fake-mode tree, or a
/// previous session's orphaned item under a recycled sid), and writing first
/// would destroy it. The carry adopts any item whose login expires later
/// than the store's — a recycled dir's orphaned item holds whatever member a
/// PREVIOUS session ended on (mid-session swaps repoint that same item), and
/// ownership cannot be proven, since no real blob carries a top-level
/// account anchor — and a tie keeps the store; otherwise the write that
/// follows replaces the orphaned login.
///
/// BUDGET: the carry's item read keeps its per-call
/// `keychain::SECURITY_TIMEOUT`, and one [`SharedSubprocessBudget`] of
/// [`SESSION_SEED_BUDGET`] spans the write, or the sign-out an absent source
/// takes instead, armed past the carry's store write and the ownership row
/// ([`owned_keychain_leg`]) so neither flock wait spends it. A stuck keychain
/// therefore spends at most the carry's read plus that budget under the
/// rotation guard the caller still holds, and the budget alone on the
/// sign-out arm, which carries nothing — the same discipline
/// [`KEYCHAIN_MIRROR_BUDGET`] holds the mirror to, re-derived into
/// [`ROTATION_LOCK_TIMEOUT`] so the start queued behind this one still waits
/// it out rather than false-refusing.
///
/// Runs AFTER the state flock (a `security` subprocess must never span it)
/// while the caller still holds the rotation guard: the session's marker and
/// registry row are stamped by then, so on macOS no rotation of a refreshable
/// chain can start past this point, and the guard keeps a rotation that
/// queued during the tree build from spending the refresh token between the
/// build and the item write that installs it.
///
/// Loud-not-fatal on every arm: a failed write leaves the session on the
/// file layer — the pre-fix behavior — and a headless box whose keychain
/// refuses must still start sessions. The one classified transient (a locked
/// keychain, `errSecInteractionNotAllowed`) arms a retry on this session's
/// watchdog credential tick, which recomputes the seed while the store still
/// matches the seed's target ([`SeedDegradeDisposition::RetryOnTick`]); every
/// other failure leaves the re-run to the next start on the same tree, or a
/// later swap onto another member.
///
/// Returns the seed's retry target — the install source the seed wrote
/// against — while a classified locked-keychain failure or a refusal over a
/// sweep's still-in-flight delete left the session's item unwritten, for the
/// watchdog's credential tick to re-run the seed against
/// ([`retry_seeded_keychain_item`]); `None` on every other outcome,
/// the pre-fix degrade included.
#[cfg(target_os = "macos")]
fn seed_session_keychain_item(
    canonical: &Path,
    runtime: &Path,
    name: &ProfileName,
    session: &SessionId,
) -> Option<PathBuf> {
    if !crate::keychain::enabled() {
        return None;
    }
    let arm = session_seed_arm(
        canonical.exists(),
        crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(canonical)
            .ok()
            .as_ref(),
    );
    match arm {
        SessionSeedArm::SignOut => {
            let sign_out = owned_keychain_leg(
                SESSION_SEED_BUDGET,
                || namespaced_keychain_ledger::authorize_write(runtime, name, session),
                |owned| crate::keychain::keychain_sign_out_for_config_dir(runtime, owned),
            );
            if let Err(e) = sign_out {
                logline!(
                    "clauth: session {} started on {}, which stores no Claude login, but signing \
                     its per-session Keychain item out failed: {e:#}. The session's Claude Code may \
                     keep serving whatever login that item still holds",
                    session.as_str(),
                    name
                );
            }
            None
        }
        SessionSeedArm::Skip => None,
        SessionSeedArm::Carry => {
            let (what, consequence, e) =
                match crate::claude::carry_session_item_into(canonical, runtime) {
                    Ok(()) => {
                        let write = owned_keychain_leg(
                            SESSION_SEED_BUDGET,
                            || namespaced_keychain_ledger::authorize_write(runtime, name, session),
                            |owned| {
                                crate::claude::keychain_mirror_source_for_config_dir(
                                    canonical, runtime, owned,
                                )
                            },
                        );
                        match write {
                            Ok(()) => {
                                // A second live session now holds this rotating
                                // login: warn, then return. The failed arm below
                                // created no copy and must not reach the
                                // success-shaped warning.
                                if let Some(line) = fanout_warning(true, name, canonical, session) {
                                    logline!("{line}");
                                }
                                return None;
                            }
                            Err(e) => (
                                "writing its per-session Keychain item",
                                "Its Claude Code falls back to the runtime credentials file, where \
                             its next token refresh migrates into the item and strands the stored \
                             refresh token; the next start on this tree re-runs the write",
                                e,
                            ),
                        }
                    }
                    Err(e) => (
                        "carrying its per-session Keychain item's pair back into the store",
                        "The item was left untouched, so the session's Claude Code reads whatever \
                     login it holds and falls back to the runtime credentials file when it holds \
                     none",
                        e,
                    ),
                };
            seed_degraded(session, name, canonical, what, consequence, &e)
        }
    }
}

/// Log one of the seed's loud-not-fatal leg failures and decide whether it
/// arms the watchdog-tick retry, returning the seed's target store for that
/// retry to guard on. The classified transient and a refusal over a sweep's
/// still-in-flight delete append the retry clause to the event line, each
/// with its own clears wording; every other failure renders the pre-fix line
/// byte-for-byte. macOS-only like its caller.
#[cfg(target_os = "macos")]
fn seed_degraded(
    session: &SessionId,
    name: &ProfileName,
    canonical: &Path,
    what: &str,
    consequence: &str,
    e: &anyhow::Error,
) -> Option<PathBuf> {
    if let Some(clears) = retry_on_tick_clears(e) {
        logline!(
            "clauth: session {} started on {} but {} failed: {e:#}. {}; the watchdog retries the \
             seed on this session's credential ticks while its store stays the seed's target, \
             {clears}",
            session.as_str(),
            name,
            what,
            consequence
        );
        return Some(canonical.to_path_buf());
    }
    logline!(
        "clauth: session {} started on {} but {} failed: {e:#}. {}",
        session.as_str(),
        name,
        what,
        consequence
    );
    None
}

/// The watchdog-tick half of the seed's retry disposition: re-run the seed's
/// carry-then-write against the recorded target while the session's store
/// still resolves to it. macOS-only like the seed; called from the credential
/// leg, OUTSIDE any state-flock hold (the carry takes its own, and a
/// `security` subprocess must never span one), its write under one
/// [`SharedSubprocessBudget`] of [`SESSION_SEED_BUDGET`] per attempt, armed
/// past both flock waits as the seed arms it.
///
/// The guard is the store comparison: a mid-session swap repoints the swap
/// cell's store and its own keychain legs own the item from there, so
/// re-running the seed past that point would write the launch member's store
/// into an item the session's file layer no longer matches. A retry that
/// fails on anything other than the classified transient drops the retry and
/// says so — the pre-fix degrade, one line, once.
#[cfg(target_os = "macos")]
fn retry_seeded_keychain_item(swap: &SessionSwap, seed_retry: &std::sync::Mutex<Option<PathBuf>>) {
    let mut retry = seed_retry.lock().unwrap_or_else(|p| p.into_inner());
    let Some(target) = retry.take() else {
        return;
    };
    if swap.canonical() != target {
        return;
    }
    let failed = match crate::claude::carry_session_item_into(&target, &swap.runtime) {
        Ok(()) => owned_keychain_leg(
            SESSION_SEED_BUDGET,
            || {
                namespaced_keychain_ledger::authorize_write(
                    &swap.runtime,
                    &ProfileName::from(swap.member()),
                    &swap.session,
                )
            },
            |owned| {
                crate::claude::keychain_mirror_source_for_config_dir(&target, &swap.runtime, owned)
            },
        )
        .err(),
        Err(e) => Some(e),
    };
    // The retry's write outcome feeds the fan-out decision: a re-seed that
    // landed copied the rotating pair into a second live session's item, a
    // failed one created no copy.
    if let Some(line) = fanout_warning(
        failed.is_none(),
        &ProfileName::from(swap.member()),
        &target,
        &swap.session,
    ) {
        logline!("{line}");
    }
    match failed {
        None => {
            logline!("clauth: re-seeded the per-session Keychain item after the keychain unlocked")
        }
        Some(e) => {
            if retry_on_tick_clears(&e).is_some() {
                // Still locked, or the sweep's delete still in flight: stay
                // armed for the next tick. The seed's own line already named
                // the retry, so a retrying attempt adds nothing — one line at
                // degrade time, none per tick.
                *retry = Some(target);
            } else {
                logline!(
                    "clauth: retrying the per-session Keychain item seed failed: {e:#}. Its \
                     Claude Code keeps the runtime credentials file, where its next token refresh \
                     migrates into the item and strands the stored refresh token; the next start \
                     on this tree re-runs the write"
                );
            }
        }
    }
}

fn reconcile_credentials(runtime_path: &Path, canonical: &Path, mode: LinkMode) -> Result<()> {
    let wrote = match mode {
        LinkMode::Real => {
            let wrote = sync_credentials_unlocked(runtime_path, canonical)?;
            let meta = runtime_path.symlink_metadata().ok();
            if !meta.is_some_and(|m| m.file_type().is_symlink() || m.is_file())
                && canonical.exists()
            {
                create_symlink(canonical, runtime_path)?;
            }
            wrote
        }
        LinkMode::Fake => mirror_credentials(runtime_path, canonical)?,
    };
    if wrote {
        // The build drained a session-side `/login` into the store: mark the
        // account's `/profile` reading stale (no network call). This runs inside
        // `acquire_synced`'s hold, and the durable-stamp removal takes no ranked
        // lock, so it is legal there.
        crate::usage::mark_profile_read_stale(canonical);
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
    let canonical = swap.canonical();
    let wrote = match swap.mode {
        LinkMode::Real => with_state_lock(|_held| {
            let wrote = sync_credentials_unlocked(&link, &canonical)?;
            Ok::<_, anyhow::Error>(wrote)
        })?,
        LinkMode::Fake if swap.isolation == Isolation::Isolated => {
            with_state_lock(|_held| mirror_credentials(&link, &canonical))?
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
            with_state_lock(|_held| mirror_credentials(&link, &canonical))?
        }
    };
    if wrote {
        // A session-side `/login` may have been copied into the store: mark the
        // account's `/profile` reading stale (no network call); the next
        // `/profile` read re-anchors from the body it gets.
        crate::usage::mark_profile_read_stale(&canonical);
    }
    Ok(())
}

/// If Claude Code's internal refresh replaced `<runtime>/.credentials.json` with
/// a regular file, copy its bytes into canonical creds and swap the file back to
/// a symlink so canonical stays the single source of truth. Returns `true` when
/// the copied claude OAuth login changed (access or refresh token), which is what
/// the copy-back's `/profile`-stale mark keys on — a rewrite that touched only
/// other keys (an MCP-server token) returns `false`. Real-symlink mode only —
/// fake mode uses [`mirror_credentials`].
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
    Ok(drain_session_login(link_path, canonical)? == Drain::LoginCopied)
}

/// The refusal a swap answers when the drain cannot judge the session's own
/// file: the outgoing `member`'s spent record is unreadable.
fn drain_unjudged(member: &str) -> SwapRefused {
    SwapRefused::OutgoingRecordUnreadable(member.to_string())
}

/// The log line a swap refusal announces. Every refusal blames the intended
/// member, except an unreadable outgoing record, which is the session's own
/// member's problem and is named as such.
fn refusal_line(session: &str, current: &str, intended: &str, why: &SwapRefused) -> String {
    if matches!(why, SwapRefused::OutgoingRecordUnreadable(_)) {
        format!("clauth: session {session} stays on {current}: {why}")
    } else {
        format!("clauth: session {session} stays on {current}: {intended} is not swappable ({why})")
    }
}

/// What [`drain_session_login`] did with a session's runtime credential file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// Nothing to drain, or the file's login was judged and the session
    /// relinked with the store's login unchanged.
    Unchanged,
    /// A session-side login of a changed account reached the store, and the
    /// session was relinked.
    LoginCopied,
    /// The spent record beside the store cannot judge the file's login: both
    /// sides are left as they are, and nothing may relink the session away
    /// from what may be the account's only live refresh token.
    Unjudged,
}

/// [`sync_credentials_unlocked`]'s body, telling a drain that could not judge
/// the runtime login apart from one that settled it: the swap executor must
/// not repoint the link over the first.
fn drain_session_login(link_path: &Path, canonical: &Path) -> Result<Drain> {
    debug_assert!(
        cfg!(test) || crate::lockorder::holds::<crate::lockorder::rank::State>(),
        "sync_credentials_unlocked without the state flock races acquire/switch \
         credential writes"
    );
    let Ok(meta) = link_path.symlink_metadata() else {
        return Ok(Drain::Unchanged);
    };
    if meta.file_type().is_symlink() {
        return Ok(Drain::Unchanged);
    }
    let runtime_bytes = std::fs::read(link_path).context("failed to read live credentials")?;
    // Skip if CC's write is mid-flight (partial, invalid, or empty object).
    // {} deserializes as ClaudeCredentials { claude_ai_oauth: None } because
    // the field is Option — require Some to confirm a completed write.
    let Ok(runtime_creds) = serde_json::from_slice::<ClaudeCredentials>(&runtime_bytes) else {
        return Ok(Drain::Unchanged);
    };
    if runtime_creds.claude_ai_oauth.is_none() {
        return Ok(Drain::Unchanged);
    }
    let canonical_bytes = std::fs::read(canonical).ok();
    let differs = canonical_bytes.as_deref() != Some(runtime_bytes.as_slice());
    // CLA-SPLIT: a static session token never rotates, so a differing runtime
    // file is a session-side re-login — never adopt it over the token (that
    // would clobber the long-lived login with a rotating chain). Keep
    // canonical and relink; the re-login stays recoverable in the runtime
    // file's lineage, and `clauth login` is the supported way to refresh the
    // profile's usage OAuth pair.
    //
    // What the session's Claude Code saved beside the static token (a
    // `/design-login`, an MCP login, a refresh of either) is the member's own:
    // with `preserve_non_login_keys` on it is synced into the sidecar before
    // the file goes, only when the runtime login still IS the token.
    if differs
        && canonical
            .file_name()
            .is_some_and(|f| f == "session-token.json")
    {
        if crate::claude::preserve_non_login_keys() {
            crate::claude::sync_live_extra_best_effort(link_path, canonical);
        }
        logline!(
            "clauth: watchdog kept the static session token \
             (a session-side re-login is never adopted over it)"
        );
        relink_to_canonical(link_path, canonical)?;
        return Ok(Drain::Unchanged);
    }
    let mut wrote = false;
    if differs {
        // Bytes differ. The keep-canonical-vs-adopt-runtime decision (write
        // recency primary, `expires_at` as the tie-break) lives in
        // `resolve_credential_winner` — see its doc for why mtime, not expiry,
        // is the signal.
        let canonical_creds = canonical_bytes
            .as_deref()
            .and_then(|cb| serde_json::from_slice::<ClaudeCredentials>(cb).ok());
        let canonical_exp = canonical_creds.as_ref().and_then(|c| {
            c.claude_ai_oauth
                .as_ref()
                .map(|o| o.expires_at.unwrap_or(0))
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
        let copy = crate::oauth::session_login_copy(canonical, &runtime_creds);
        if copy == crate::oauth::SessionLoginCopy::Unknown {
            // The record beside the store cannot say whether the runtime login
            // is the chain head or a dead token: write neither side, and leave
            // the session on its own file rather than relinking it away from
            // what may be the account's only live refresh token.
            logline!(
                "clauth: watchdog left both credential copies alone (cannot read the spent-token \
                 record beside the store)"
            );
            return Ok(Drain::Unjudged);
        }
        if copy == crate::oauth::SessionLoginCopy::Spent {
            // The runtime login's refresh token is one the server already
            // consumed: copying it back would rewind the chain onto a dead
            // token.
            logline!(
                "clauth: watchdog kept the account's credentials \
                 (the session copy holds a used refresh token)"
            );
        } else if resolve_credential_winner(
            canonical_exp,
            runtime_exp,
            canonical_mtime,
            runtime_mtime,
        ) {
            // Canonical written at/after the runtime re-login (or wins the
            // tie-break); don't overwrite it with the runtime bytes.
            logline!(
                "clauth: watchdog kept canonical credentials \
                 (canonical written more recently than runtime); \
                 not overwriting with runtime re-login bytes"
            );
        } else {
            atomic_write_600(canonical, &runtime_bytes)?;
            // Mark the `/profile` reading stale only when the copied LOGIN
            // changed, never for a rewrite that touched only other keys (an
            // MCP-server token refresh).
            wrote = crate::profile::login_changed(canonical_creds.as_ref(), &runtime_creds);
        }
    }
    relink_to_canonical(link_path, canonical)?;
    Ok(if wrote {
        Drain::LoginCopied
    } else {
        Drain::Unchanged
    })
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
/// single source of truth, through the same staged publish every other
/// credential link takes, so a sibling session never sees the path missing. If
/// canonical is gone, removes the file.
fn relink_to_canonical(link_path: &Path, canonical: &Path) -> Result<()> {
    if canonical.exists() {
        crate::claude::publish_credential_link(link_path, canonical)
    } else {
        std::fs::remove_file(link_path)?;
        Ok(())
    }
}

/// Apply the adoption drain's login rules to a regular-file saved login F
/// (`link_path`) for `member`, in order; the first that decides acts. C is the
/// member's `credentials.json` (the rotating store) and T its
/// `session-token.json` sidecar. Runs inside the state flock with the member's
/// rotation guard held, after every marker claim succeeded, so any refusal
/// rolls the claims back.
///
///  1. RE-SAVE: F's refresh token equals C's — with a non-blank access token,
///     write F's login into C through the key-preserving writer, carry F's
///     non-login keys and relink onto C; with a blank one, write no login,
///     carry nothing (the carry matches a live login against the store's own,
///     and a blank access is no login) and relink onto C. Or F is T's current
///     static token — relink onto T and carry non-login keys, never writing
///     T's login. A rotation staged beside C takes no login write either, so
///     the staged pair still reads as newer: F's refresh token is C's already,
///     so nothing is written to C, and the carry stands down there itself,
///     logging that F's non-login keys (a design login, MCP logins) go with
///     the relink.
///  2. OLDER LOGIN: F's non-blank refresh token is in C's spent record — carry
///     F's non-login keys into C, relink onto C, write no login. An unreadable
///     record refuses.
///  3. STATIC TOKEN OR LOGGED-OUT SHELL: F has no non-blank refresh token and
///     matches no store — merge F's carried keys (MCP logins) into the member's
///     install source, only a strictly fresher copy per server winning, then
///     relink onto it. F's login is written nowhere, and no key that
///     authenticates or names an account crosses: F can be a stale row's copy
///     of another account. A store copy newer than F's is never replaced, and
///     only a strictly fresher one from F lands — its expiry later than the
///     store's, or present where the store has none — since peer sessions write
///     the store through their links.
///  4. NO STORE HOLDS IT: F's non-blank refresh token is neither C's current
///     login nor in C's spent record — refuse, writing nothing and keeping F
///     byte for byte.
fn adopt_saved_login(
    link_path: &Path,
    member: &ProfileName,
    held: &crate::lock::StateLockHeld,
) -> Result<()> {
    let f_bytes = std::fs::read(link_path).context("failed to read the session's saved login")?;
    let Ok(f_creds) = serde_json::from_slice::<ClaudeCredentials>(&f_bytes) else {
        // A partial write is Claude Code's mid-flight: leave the file alone
        // until the write completes; the own-stores gate refuses it for now.
        return Ok(());
    };
    let Some(login) = f_creds.claude_ai_oauth.as_ref() else {
        // No login: leave the file alone; the own-stores gate refuses it.
        return Ok(());
    };

    let member_dir = profile_dir(member)?;
    let c_path = member_dir.join("credentials.json");
    let c_creds = std::fs::read(&c_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeCredentials>(&bytes).ok());

    // A blank token is an absent token: an empty refresh makes a static token,
    // and an empty access is no login to re-save (Claude Code's dead-token wipe
    // blanks both).
    let f_access = login.access_token.as_str();
    let f_refresh = login.refresh_token.as_deref().filter(|r| !r.is_empty());

    // Rule 1: C's current login (same non-blank refresh token, never bytes).
    if let Some(r) = f_refresh
        && c_creds.as_ref().and_then(|c| c.refresh_token()) == Some(r)
    {
        if f_access.trim().is_empty() {
            // The logged-out wipe blanks the access token of a login C still
            // holds. Writing that blank over C's live token would break the
            // store, and the carry matches a live login against the store's
            // own — a blank one matches none — so it carries nothing either.
            relink_to_canonical(link_path, &c_path)?;
            return Ok(());
        }
        if crate::profile::rotation_staged_beside(&c_path) {
            // Writing C would move it past the staged pair, which the next load
            // then discards. F's refresh token is C's already, so no login is
            // lost, but a non-login key only F holds goes with the relink: the
            // sync stands down and says so.
            if crate::claude::preserve_non_login_keys() {
                crate::claude::sync_live_extra_best_effort(link_path, &c_path);
            }
            relink_to_canonical(link_path, &c_path)?;
            return Ok(());
        }
        crate::profile::write_profile_credentials(member, Some(&f_creds), held)?;
        if crate::claude::preserve_non_login_keys() {
            crate::claude::sync_live_extra_best_effort(link_path, &c_path);
        }
        relink_to_canonical(link_path, &c_path)?;
        return Ok(());
    }

    // Rule 1: T's current static token (no refresh, the same token T installs).
    if f_refresh.is_none()
        && !f_access.trim().is_empty()
        && crate::claude::installed_session_token(member).as_deref() == Some(f_access)
    {
        let t_path = crate::claude::install_source_path(member)?;
        if crate::claude::preserve_non_login_keys() {
            crate::claude::sync_live_extra_best_effort(link_path, &t_path);
        }
        relink_to_canonical(link_path, &t_path)?;
        return Ok(());
    }

    if f_refresh.is_none() {
        // Rule 3: a static token or logged-out shell matching no store. Its
        // login is never written anywhere; the tree relinks to the member's
        // install source, and a missing one refuses before the relink touches
        // F. Its carried keys (MCP logins) merge in first, fresher copy
        // winning: they belong to no Claude account and the file may be their
        // only home, while a peer session writing the store through its link
        // may hold the newer copy of one. Only the carried keys cross — F is no
        // account's login and can be a stale row's copy of another account,
        // whose design login or device token must not ride in.
        let install = crate::claude::install_source_path(member)?;
        guard_relink_target(member, &install)?;
        crate::claude::carry_live_extra_fresher_best_effort(
            link_path,
            &install,
            crate::claude::preserve_non_login_keys(),
        );
        relink_to_canonical(link_path, &install)?;
        return Ok(());
    }

    // Rules 2 and 4, judged through the store's spent record.
    match crate::oauth::session_login_copy(&c_path, &f_creds) {
        crate::oauth::SessionLoginCopy::Spent => {
            // Rule 2: an older login. The store it relinks onto must exist
            // before any carry, or the refused adoption would delete the
            // session's file through the relink's remove branch.
            guard_relink_target(member, &c_path)?;
            if crate::claude::preserve_non_login_keys() {
                crate::claude::sync_live_extra_matching_best_effort(
                    link_path,
                    &c_path,
                    Some(f_access),
                );
            }
            relink_to_canonical(link_path, &c_path)?;
            Ok(())
        }
        crate::oauth::SessionLoginCopy::Unknown => {
            anyhow::bail!(
                "the session's saved login cannot be judged: {member}'s \
                 spent-token record cannot be read, so the session keeps \
                 its own copy until the record reads again"
            );
        }
        crate::oauth::SessionLoginCopy::Copyable => {
            // Rule 4: a refreshed login no store holds. Nothing proves whose
            // refresh chain it is, so refuse without writing anything.
            anyhow::bail!(
                "the session's saved login holds a refresh token none of \
                 {member}'s stores hold; refusing to adopt it"
            );
        }
    }
}

/// Refuse before a relink whose target store is gone, so a refused adoption
/// never reaches [`relink_to_canonical`]'s remove branch and deletes the
/// session's file. The refusal names the store's file, since the two stores one
/// member can relink onto are told apart by nothing else in the message.
fn guard_relink_target(member: &ProfileName, store: &Path) -> Result<()> {
    if !store.exists() {
        let file = store
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        anyhow::bail!("{member} has no {file} to relink the session onto; refusing to adopt it");
    }
    Ok(())
}

/// Bidirectional mtime mirror between `runtime/.credentials.json` and canonical
/// creds: "latest mtime wins", newer side copied over older. Skips partial
/// writes (invalid JSON). Fake-symlink mode only. Returns `true` only for a
/// session→store copy whose claude OAuth login changed; a store→session copy
/// writes no store and returns `false`.
fn mirror_credentials(runtime_path: &Path, canonical: &Path) -> Result<bool> {
    // Same flock requirement and same test ceiling as `sync_credentials_unlocked`.
    debug_assert!(
        cfg!(test) || crate::lockorder::holds::<crate::lockorder::rank::State>(),
        "mirror_credentials without the state flock races acquire/switch \
         credential writes"
    );
    let runtime_meta = runtime_path.metadata().ok();
    let canonical_meta = canonical.metadata().ok();

    if let Some((src, dst)) = newer_side(runtime_path, canonical, runtime_meta, canonical_meta) {
        // A newer runtime copy whose refresh token the server already consumed
        // never reaches the store; canonical flows back over it instead, the
        // fake-mode counterpart of the real-mode relink. One the record beside
        // the store cannot judge is copied neither way.
        if src == runtime_path
            && let Ok(bytes) = std::fs::read(runtime_path)
            && let Ok(runtime_creds) = serde_json::from_slice::<ClaudeCredentials>(&bytes)
        {
            match crate::oauth::session_login_copy(canonical, &runtime_creds) {
                crate::oauth::SessionLoginCopy::Copyable => {}
                crate::oauth::SessionLoginCopy::Spent => {
                    if canonical.exists() {
                        copy_if_valid_creds(canonical, runtime_path)?;
                    }
                    return Ok(false);
                }
                crate::oauth::SessionLoginCopy::Unknown => return Ok(false),
            }
        }
        let wrote = copy_if_valid_creds(src, dst)?;
        if src == runtime_path {
            return Ok(wrote);
        }
    }
    Ok(false)
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

fn copy_if_valid_creds(src: &Path, dst: &Path) -> Result<bool> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    // Same guard as sync_credentials_unlocked: reject partial, invalid, or
    // empty-object writes before letting them stomp the canonical file.
    let Ok(creds) = serde_json::from_slice::<ClaudeCredentials>(&bytes) else {
        return Ok(false);
    };
    if creds.claude_ai_oauth.is_none() {
        return Ok(false);
    }
    let dst_bytes = std::fs::read(dst).ok();
    if dst_bytes.as_deref() == Some(bytes.as_slice()) {
        return Ok(false);
    }
    let dst_creds = dst_bytes
        .as_deref()
        .and_then(|b| serde_json::from_slice::<ClaudeCredentials>(b).ok());
    atomic_write_600(dst, &bytes).with_context(|| format!("failed to write {}", dst.display()))?;
    // Mark the `/profile` reading stale only when the copied LOGIN changed.
    Ok(crate::profile::login_changed(dst_creds.as_ref(), &creds))
}

/// Walk both `~/.claude/` and the runtime tree; copy the newer bytes onto the
/// older, seeding one-sided files onto the other — CC may create runtime-side
/// state (project history, scratch files) and the user may add `~/.claude/`
/// entries between ticks, both must propagate. **No deletion**: a file missing
/// from one side is "not yet seen", never "intentionally removed", so the mirror
/// never destroys data. Top-level `settings.json`, `.credentials.json`,
/// `.claude.json` and `jobs` are skipped (settings is a rewritten copy;
/// credentials has its own stricter mirror; `.claude.json` is reconciled
/// per-profile; `jobs` never enters a copy-host tree either way). Fake-symlink
/// mode only.
///
/// The walk closes with one pass over [`AliasClasses`], which is what keeps two
/// `~/.claude` names resolving to ONE file from letting sort order decide whose
/// bytes survive.
fn mirror_tree(claude_home: &Path, runtime: &Path) -> Result<()> {
    // `.claude.json` is a per-profile copy reconciled by `crate::claude_json`,
    // not part of the `~/.claude/` tree — skip it here so the tree mirror never
    // copies it into `~/.claude/.claude.json`. `jobs` is skipped too: a copy
    // host never carries the job store either way (cloudy 2026-10-22), so a
    // tree's copy of a `claude rm`ed job is never published back into the store
    // and a store job never comes back out of a tree.
    let skip_top: HashSet<&str> = ["settings.json", ".credentials.json", ".claude.json", "jobs"]
        .into_iter()
        .collect();
    // The one `canonicalize` this walk pays for: every entry below inherits it.
    let root_key = crate::canonical::identity(claude_home).ok();
    let mut classes = AliasClasses::default();
    for name in union_children(claude_home, runtime) {
        // The supervisor's own state never crosses either way: the build walk
        // skips it, so the mirror must too, or a tick would copy the home
        // roster into the tree and the tree's roster back out.
        if name.to_str().is_some_and(|n| skip_top.contains(n)) || is_daemon_state_name(&name) {
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
        return crate::canonical::identity(a).ok();
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
        Ok(m) if m.file_type().is_symlink() => {
            crate::canonical::plain(p).unwrap_or_else(|_| p.to_path_buf())
        }
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

// ── codex session homes ──────────────────────────────────────────────────────
//
// The codex runtime mirrors the claude runtime: per-session under the real
// transport, keyed by the same minted sid, marker-flock liveness in the dirs
// `session_marker_paths` probes — `sessions[-isolated]-<sid>` under the real
// transport, the bare stems under the fake one (which is what makes
// `has_live_session` — and so delete/disable/rotation gating — work for codex
// with no harness awareness), registered in the same live-session registry
// with the codex tag. What it does NOT mirror: no swap executor, no
// settings/claude-json watchdog legs, and no wipe of the profile-global store.

/// The stem of a codex home of this flavor. Both spell a `codex-home` prefix,
/// so [`is_codex_home_dir_name`] covers every variant, and neither matches the
/// `runtime*`/`sessions*` predicates GC and the config reconcilers act on.
fn codex_home_stem(isolation: Isolation) -> &'static str {
    match isolation {
        Isolation::Shared => CODEX_HOME_STEM,
        Isolation::Isolated => "codex-home-isolated",
    }
}

/// The `(home, sessions)` dir names a codex session of this flavor uses under
/// this transport — [`paired_dir_names`]'s codex twin, with the same
/// [`LinkMode::Fake`] collapse to the bare stems. Under the bare SHARED stem
/// the home is the profile-global store itself, which is the fake-mode sharing
/// story in one line: no symlinks, so the durable state is simply lived in.
fn codex_paired_dir_names(isolation: Isolation, session: &str, mode: LinkMode) -> (String, String) {
    let suffix = match mode {
        LinkMode::Real => format!("-{session}"),
        LinkMode::Fake => String::new(),
    };
    (
        format!("{}{suffix}", codex_home_stem(isolation)),
        format!("{}{suffix}", isolation.sessions_stem()),
    )
}

/// Whether `name`'s sessions run under the fake-symlink transport — where a
/// codex session home holds a COPY of `auth.json`, not a symlink to the
/// store. The codex standby refresh is the ONLY caller (the daemon's
/// background adoption probes the mode under the state lock in `acquire_tree`
/// instead). Under real symlinks a live
/// session reads the very file a rotation writes (codex reloads before
/// spending, the guard serializes), but under fake mode it holds a separate
/// carrier, so a store rotation would spend a token the session still holds.
/// A missing profile dir or a probe error reads `false` — not fake — so the
/// refresh proceeds; the caller already checked for a live session, which is
/// what guarantees the profile dir exists.
pub(crate) fn profile_uses_fake_transport(name: &str) -> bool {
    let Ok(dir) = profile_dir(&ProfileName::from(name)) else {
        return false;
    };
    matches!(detect_link_mode(&dir), Ok(LinkMode::Fake))
}

/// The durable per-profile codex store, `profiles/<name>/codex-home` — the
/// symlink target for the state that must outlive one session (the sqlite
/// stores, `history.jsonl`, the rollout roots).
fn codex_global_home(name: &str) -> Result<PathBuf> {
    profile_subpath(&ProfileName::from(name), CODEX_HOME_STEM)
}

/// The config keys a clauth-built codex home must not inherit, whatever the
/// operator set for their own `~/.codex`. Each one lets a session read or write
/// outside the boundary the home exists to draw:
///
/// - `sqlite_home` moves the state DBs, so every profile's
///   goals/logs/memories/state land in one directory and the home's durable
///   links are never opened through.
/// - `cli_auth_credentials_store` makes codex ignore the linked `auth.json` and
///   delete it on the first refresh (decision 6).
/// - `debug.config_lockfile` replays a lockfile as the WHOLE effective config:
///   `ConfigLayerStack::new(vec![lock_layer], ..)` (codex `core/src/config/mod.rs`)
///   rebuilds from one layer, so the `-c` layer carrying both forced overrides
///   is not outranked but ERASED.
///
/// The first two are also pinned by a forced `-c` at spawn. Stripping them here
/// too is deliberate: the third key is what makes a `-c` alone insufficient, and
/// a defense that only holds while the layer stack survives is not one.
const CODEX_CONFIG_STRIP_KEYS: &[&str] = &["sqlite_home", "cli_auth_credentials_store"];
const CODEX_CONFIG_STRIP_SUBKEYS: &[(&str, &str)] = &[("debug", "config_lockfile")];

/// Copy the operator's `config.toml` into a session home with the escape keys
/// removed. An unparseable config copies through verbatim: codex will reject it
/// the same way, and a session that cannot start is a better answer than one
/// silently reshaped by a parse this function got wrong. Comments and key order
/// are lost in the rewrite, which costs nothing — the copy is codex's to write
/// in place and is discarded at teardown, never synced back over the original.
///
/// Every branch lands owner-only: the copy sits in a tree the perms sweep
/// stops short of, so the operator's own mode would otherwise be what it keeps.
fn copy_codex_config(src: &Path, dst: &Path) -> Result<()> {
    let raw = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    let bytes = match stripped_codex_config(&raw) {
        Some(parsed) => toml::to_string(&parsed)
            .with_context(|| format!("failed to re-render {}", src.display()))?
            .into_bytes(),
        None => raw,
    };
    crate::profile::atomic_write_600(dst, bytes)
        .with_context(|| format!("failed to write {}", dst.display()))
}

/// The parsed config with the escape keys removed, or `None` when there was
/// nothing to strip or the bytes do not parse, so the copy goes verbatim.
fn stripped_codex_config(raw: &[u8]) -> Option<toml::Value> {
    let mut parsed = toml::from_str::<toml::Value>(str::from_utf8(raw).ok()?).ok()?;
    let table = parsed.as_table_mut()?;
    let mut stripped = false;
    for key in CODEX_CONFIG_STRIP_KEYS {
        stripped |= table.remove(*key).is_some();
    }
    for (parent, key) in CODEX_CONFIG_STRIP_SUBKEYS {
        if let Some(sub) = table.get_mut(*parent).and_then(toml::Value::as_table_mut) {
            stripped |= sub.remove(*key).is_some();
        }
    }
    stripped.then_some(parsed)
}

/// The sqlite stores (with their `-wal`/`-shm` companions, per the plan's
/// table) plus the history file — per-session paths symlinked into the
/// profile-global home so memory survives session teardown. The companions
/// ride along deliberately: a write through a dangling symlink CREATES its
/// target, so the bytes land in the store whichever path sqlite derives the
/// auxiliary names from — linking them makes the layout correct without
/// betting on sqlite's symlink resolution. A store name a future codex adds
/// stays per-session until this list learns it — a bounded, visible
/// degradation, against silently linking anything.
const CODEX_DURABLE_ENTRIES: &[&str] = &[
    "goals_1.sqlite",
    "goals_1.sqlite-wal",
    "goals_1.sqlite-shm",
    "logs_2.sqlite",
    "logs_2.sqlite-wal",
    "logs_2.sqlite-shm",
    "memories_1.sqlite",
    "memories_1.sqlite-wal",
    "memories_1.sqlite-shm",
    "state_5.sqlite",
    "state_5.sqlite-wal",
    "state_5.sqlite-shm",
    "thread_history_1.sqlite",
    "thread_history_1.sqlite-wal",
    "thread_history_1.sqlite-shm",
    "history.jsonl",
];

/// The rollout roots a codex home keeps side by side: live threads under
/// `sessions/`, and the ones the operator archived under `archived_sessions/`
/// (codex `rollout/src/lib.rs`). Both link into the profile-global home the
/// way the durable entries do, so a rollout is in the store the moment codex
/// writes it and every later session lists it. Archiving MOVES a rollout
/// between them, so linking only the first would rename an archived thread
/// out of the store into the per-session home, and "archive this thread"
/// would mean "delete it at teardown" — the one operation whose whole promise
/// is that the thread is kept.
const CODEX_ROLLOUT_ROOTS: &[&str] = &["sessions", "archived_sessions"];

/// The operator-`~/.codex` entries a SHARED codex session sees — instruction
/// and extension surfaces, same reasoning as the claude shared runtime linking
/// the operator's `~/.claude`. `hooks.json` is deliberately absent: hooks
/// execute code inside a home clauth built, so linking it is a per-profile
/// opt-in ([`CodexProfileOpts::hooks_json`]), never a default.
const CODEX_OPERATOR_ENTRIES: &[&str] = &[
    "skills",
    "rules",
    "agents",
    "templates",
    "references",
    "AGENTS.md",
    // Installed plugins (`plugins/cache`, `plugins/data` — codex
    // `core-plugins/src/store.rs`) are the operator's tooling, the same family
    // as skills and rules, so they are LINKED rather than made durable
    // per-profile: an install reaches every profile and outlives the session
    // that made it. Unlinked they were neither, and a plugin installed inside a
    // clauth session died with the home.
    "plugins",
];

/// Which side alone could have written since the last convergence — the
/// tie-breaker once neither `last_refresh` nor mtime separates the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConvergePrior {
    /// Between sessions only the store is written (the standby leg, a
    /// capture). A same-flavor session live beside this build writes the copy
    /// stamped, so the stamp branch settles it first: the prior only breaks a
    /// full tie no live writer produces.
    Build,
    /// The session was the only live writer (the standby leg stands down for
    /// a live fake-transport session), and it writes the copy.
    Teardown,
}

/// Converge the profile store and a fake-mode home's `auth.json` copy at a
/// session boundary; content-equal is a no-op. The chain's own event decides:
/// the later `last_refresh` wins, mtime only where a side has no parseable
/// stamp, and a remaining tie goes to the boundary's [`ConvergePrior`]. A tie
/// sent to the store hands a SPENT refresh token back to a copy the session
/// just rotated — the permanent-death shape on a coarse-mtime filesystem — so
/// which side wins a tie is a fact about who could have written, not a
/// default. One direction is never enough: store→copy alone never carries a
/// rotation out, copy→store alone undoes a re-capture at the next start. The
/// claude fake transport mirrors per tick; codex has no watchdog, so the
/// boundaries are its convergence points and mid-session divergence its cost.
fn converge_fake_codex_auth(store: &Path, copy: &Path, prior: ConvergePrior) -> Result<()> {
    match (store.exists(), copy.exists()) {
        (false, false) => Ok(()),
        (true, false) => copy_file(store, copy),
        // The store must never lag a chain that exists only in the copy: the
        // rotation and capture machinery read the STORE.
        (false, true) => copy_file(copy, store),
        (true, true) => {
            if files_match(store, copy).unwrap_or(false) {
                return Ok(());
            }
            let copy_wins = match (last_refresh_ms_of(copy), last_refresh_ms_of(store)) {
                (Some(c), Some(s)) if c != s => c > s,
                _ => match (file_mtime(copy), file_mtime(store)) {
                    (Some(c), Some(s)) if c != s => c > s,
                    _ => prior == ConvergePrior::Teardown,
                },
            };
            if copy_wins {
                copy_file(copy, store)
            } else {
                copy_file(store, copy)
            }
        }
    }
}

/// The `last_refresh` stamp of an `auth.json` on disk, `None` for a file that
/// does not parse or carries none.
fn last_refresh_ms_of(path: &Path) -> Option<i64> {
    let bytes = std::fs::read(path).ok()?;
    crate::codex_auth::CodexAuth::parse(&bytes)
        .ok()?
        .last_refresh_ms()
}

/// The per-profile codex knobs, read from the profile's own `config.toml` —
/// the file the reload fingerprint already walks. A codex profile's config is
/// never read by the claude `load_profile`, so this minimal shape is its one
/// reader and unknown keys stay tolerated.
#[derive(Default, serde::Deserialize)]
struct CodexProfileOpts {
    /// Link the operator's `~/.codex/hooks.json` into shared session homes.
    #[serde(default)]
    hooks_json: bool,
}

fn codex_profile_opts(name: &str) -> CodexProfileOpts {
    let Ok(path) = profile_subpath(&ProfileName::from(name), "config.toml") else {
        return CodexProfileOpts::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => toml::from_str(&raw).unwrap_or_default(),
        Err(_) => CodexProfileOpts::default(),
    }
}

/// Build one codex session home. Additive over an existing tree, like the
/// claude build: a link already in place is left alone, so a rebuild over a
/// live shared tree cannot yank entries out from under a sibling.
///
/// The table (from the codex plan):
/// - `auth.json` — under real symlinks, BOTH flavors link the profile's own
///   `profiles/<name>/auth.json`: one physical file is what makes concurrent
///   carriers safe (codex's own reload-and-skip handles codex-vs-codex, the
///   rotation guard handles clauth-vs-codex). Under [`LinkMode::Fake`] it is
///   a copy — the same consequences the claude fake-mode credential copy
///   already documents, sharpened by codex's single-use refresh chain: the
///   copy is a second carrier whose refreshes strand the store, and a
///   fake-mode host accepts that or does not run codex sessions.
/// - `config.toml` — a COPY of the operator's (codex writes it in place, so a
///   link would mutate the operator's own file). Absent operator config copies
///   nothing.
/// - the operator surfaces ([`CODEX_OPERATOR_ENTRIES`]) — shared flavor only,
///   links (fake: copies); isolated links nothing from the operator.
/// - the durable stores ([`CODEX_DURABLE_ENTRIES`]) — shared flavor under real
///   symlinks: links into the profile-global home, dangling until codex
///   creates through them, which is the point. Isolated: per-session. Fake:
///   the home IS the global store, nothing to link.
/// - the rollout roots ([`CODEX_ROLLOUT_ROOTS`]) — shared flavor under real
///   symlinks: links into the profile-global home, their targets created
///   first (codex creates `sessions/<yyyy>/<mm>/<dd>/` through them, and a
///   `create_dir_all` cannot pass a dangling dir link). Isolated: a real
///   per-session `sessions/`, discarded by design. Fake: the home IS the
///   global store.
fn build_codex_home(home: &Path, name: &str, isolation: Isolation, mode: LinkMode) -> Result<()> {
    let operator = home_dir()?.join(".codex");
    let global = codex_global_home(name)?;
    let auth_store = profile_subpath(&ProfileName::from(name), "auth.json")?;

    let place = |src: &Path, dst: &Path| -> Result<()> {
        if dst.symlink_metadata().is_ok() || !src.exists() {
            return Ok(());
        }
        match mode {
            LinkMode::Real => link_entry(src, dst),
            LinkMode::Fake => copy_tree(src, dst),
        }
    };

    // The one physical auth.json. Placed even while the store is absent (a
    // captured login can arrive after the first start): a dangling link reads
    // as "no credentials" to codex today and as the store the moment it
    // exists. Under Fake the store and the home's copy CONVERGE at every
    // session boundary (here, and again at teardown) — see
    // [`converge_fake_codex_auth`] for why one direction is not enough.
    let auth_dst = home.join("auth.json");
    match mode {
        LinkMode::Real => {
            if auth_dst.symlink_metadata().is_err() {
                link_entry(&auth_store, &auth_dst)?;
            }
        }
        LinkMode::Fake => converge_fake_codex_auth(&auth_store, &auth_dst, ConvergePrior::Build)?,
    }

    let operator_config = operator.join("config.toml");
    if operator_config.exists() && home.join("config.toml").symlink_metadata().is_err() {
        copy_codex_config(&operator_config, &home.join("config.toml"))?;
    }
    // `codex --profile <name>` layers `$CODEX_HOME/<name>.config.toml` over the
    // base config, and CODEX_HOME is this session home — so without these the
    // flag resolves to an empty layer and silently runs the base config. They
    // are sanitized like the base file: a layer can spell the same escapes.
    if let Ok(entries) = std::fs::read_dir(&operator) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !name.ends_with(".config.toml") {
                continue;
            }
            let dst = home.join(name);
            if dst.symlink_metadata().is_err() {
                copy_codex_config(&entry.path(), &dst)?;
            }
        }
    }

    if isolation == Isolation::Shared {
        for entry in CODEX_OPERATOR_ENTRIES {
            place(&operator.join(entry), &home.join(entry))?;
        }
        if codex_profile_opts(name).hooks_json {
            place(&operator.join("hooks.json"), &home.join("hooks.json"))?;
        }
        if mode == LinkMode::Real {
            crate::profile::mkdir_700(&global)
                .with_context(|| format!("failed to create {}", global.display()))?;
            for entry in CODEX_DURABLE_ENTRIES {
                let dst = home.join(entry);
                if dst.symlink_metadata().is_err() {
                    link_entry(&global.join(entry), &dst)?;
                }
            }
            for root in CODEX_ROLLOUT_ROOTS {
                let target = global.join(root);
                crate::profile::mkdir_700(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                let dst = home.join(root);
                if dst.symlink_metadata().is_err() {
                    link_entry(&target, &dst)?;
                }
            }
        }
    }

    let sessions = home.join("sessions");
    if sessions.symlink_metadata().is_err() {
        crate::profile::mkdir_700(&sessions)
            .with_context(|| format!("failed to create {}", sessions.display()))?;
    }
    Ok(())
}

/// Live codex session guard — the codex [`ProfileRuntime`]. On drop: converges
/// a fake-mode auth copy back to the store, carries a durable store codex
/// healed in place back into the profile-global home, drops the registry row
/// and marker, and removes the per-session home (its rollout roots are links,
/// so the rollouts stay in the store); the bare `codex-home` (the durable
/// store, and the fake-mode shared home) is never removed.
pub(crate) struct CodexRuntime {
    home: PathBuf,
    sessions: PathBuf,
    pid_file: PathBuf,
    session: SessionId,
    profile: String,
    isolation: Isolation,
    mode: LinkMode,
    _pid_lock: File,
}

impl CodexRuntime {
    pub(crate) fn acquire(name: &str, isolation: Isolation) -> Result<Self> {
        let owned = ProfileName::from(name);
        let profile_root = profile_dir(&owned)?;
        // Same ordering rule as the claude acquire: RotationGuard outermost,
        // state flock inside. What it buys here is the row: `launch_store` and
        // the marker must be visible before any clauth-side codex rotation
        // (phase 4) can decide against this profile, or the rotation gate
        // reads the account as idle while a session is mid-start on it.
        let _rotation_guard = RotationGuard::acquire(&owned)?;

        let (session, home, sessions, pid_file, pid_lock, mode) = with_state_lock(|_held| {
            crate::profile::mkdir_700(&profile_root)
                .with_context(|| format!("failed to create {}", profile_root.display()))?;
            let mode = detect_link_mode(&profile_root)?;
            let mut session = SessionId::mint();
            let (mut home_name, mut sessions_name) =
                codex_paired_dir_names(isolation, session.as_str(), mode);
            for _ in 0..SID_COLLISION_REMINTS {
                let pid_file = profile_subpath(&owned, &sessions_name)?.join(session.as_str());
                if !is_session_alive(&pid_file) {
                    break;
                }
                session = SessionId::mint();
                (home_name, sessions_name) =
                    codex_paired_dir_names(isolation, session.as_str(), mode);
            }
            let home = profile_subpath(&owned, &home_name)?;
            let sessions = profile_subpath(&owned, &sessions_name)?;
            let pid_file = sessions.join(session.as_str());

            // Decision 8 lets codex sessions run concurrently because they share
            // ONE physical auth.json. Under Fake there is no sharing: the two
            // flavors collapse to DIFFERENT bare stems, each holding its own
            // COPY converged from the same store. Two live flavors are then two
            // carriers of one single-use chain with no reload-and-skip between
            // them (codex's own answer needs one inode), and the first refresh
            // on either side strands the other for good. Same flavor is fine —
            // that IS one home. Refusing is what the premise actually supports.
            if mode == LinkMode::Fake {
                let other = isolation.other();
                let (_, other_sessions_name) =
                    codex_paired_dir_names(other, session.as_str(), mode);
                let other_sessions = profile_subpath(&owned, &other_sessions_name)?;
                if other_sessions.exists() && !matches!(live_sessions_at(&other_sessions), Some(0))
                {
                    anyhow::bail!(
                        "'{name}' already has a live {other} codex session, and this host \
                         cannot symlink. The two would hold SEPARATE copies of one \
                         single-use chain, and the first refresh on either strands the \
                         other. Close the {other} session before starting this one"
                    );
                }
            }

            crate::profile::mkdir_700(&sessions)
                .with_context(|| format!("failed to create {}", sessions.display()))?;
            let active = prune_stale_sessions(&sessions).unwrap_or(1);
            // A dead session's leftovers under a recycled sid are rebuilt from
            // scratch — but ONLY a genuinely per-session home. Testing the
            // SHARED bare stem alone was not enough: under Fake every name is
            // sid-free, so the isolated home (`codex-home-isolated`) read as
            // per-session and was wiped. That home holds a physical auth.json
            // COPY, and a session that died before Drop could converge leaves
            // the rotated chain nowhere else — the wipe destroyed it, and
            // `build_codex_home` then converged the SPENT store token back out
            // for codex to replay into `refresh_token_reused`. The predicate
            // that already answers this correctly demands a session id, so
            // both bare stems keep the state that must outlive a session.
            if active == 0
                && home
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_per_session_codex_home_name)
                && home.symlink_metadata().is_ok()
            {
                std::fs::remove_dir_all(&home)
                    .with_context(|| format!("failed to clear {}", home.display()))?;
            }
            crate::profile::mkdir_700(&home)
                .with_context(|| format!("failed to create {}", home.display()))?;
            build_codex_home(&home, name, isolation, mode)?;

            let file = open_pid_file(&pid_file)
                .with_context(|| format!("failed to open {}", pid_file.display()))?;
            if let Err(e) = file.try_lock() {
                anyhow::bail!(
                    "failed to claim session marker {}: {e}. Another live process \
                     holds this session id",
                    pid_file.display()
                );
            }

            // Register inside the same hold, marker already flock-held — the
            // row never exists without a liveness signal. `launch_store` is
            // the auth.json THIS SESSION READS — which is what the rotation
            // refusal must test (the #59 review's one-liner, honored by
            // intent): under real symlinks the home's auth.json IS
            // profiles/<name>/auth.json, and under the fake transport it is
            // the copy the session actually holds, where the accepted
            // spelling would name a file the session never reads.
            let row = crate::live_sessions::LiveSession::starting(
                &session,
                name,
                crate::harness::Harness::Codex,
                isolation == Isolation::Isolated,
                false,
                crate::live_sessions::Launcher::Own,
                Some(home.join("auth.json")),
                None,
            );
            if let Err(e) = crate::live_sessions::register(&row) {
                logline!("clauth: registering the live codex session failed: {e}");
            }
            Ok::<_, anyhow::Error>((session, home, sessions, pid_file, file, mode))
        })?;

        Ok(Self {
            home,
            sessions,
            pid_file,
            session,
            profile: name.to_string(),
            isolation,
            mode,
            _pid_lock: pid_lock,
        })
    }

    /// The home this session's `CODEX_HOME` pins.
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for CodexRuntime {
    fn drop(&mut self) {
        // The teardown half of the fake-mode auth convergence: a chain the
        // session rotated in the copy reaches the store NOW, not at some next
        // start that may never come — and a rotation or capture between
        // sessions reads a store that is not stale.
        if self.mode == LinkMode::Fake
            && let Ok(store) =
                profile_subpath(&ProfileName::from(self.profile.as_str()), "auth.json")
            && let Err(e) = converge_fake_codex_auth(
                &store,
                &self.home.join("auth.json"),
                ConvergePrior::Teardown,
            )
        {
            logline!("clauth: codex auth converge at teardown failed: {e:#}");
        }
        // Carry a durable store codex healed in place back into the
        // profile-global home — best-effort, never failing a completed
        // session, and a no-op when the home IS the store (fake shared) or
        // the flavor keeps its state per-session (isolated).
        if self.isolation == Isolation::Shared
            && self.mode == LinkMode::Real
            && let Ok(global) = codex_global_home(&self.profile)
        {
            // codex's own corrupt-DB recovery RENAMES the path it was handed
            // (`state/src/runtime/recovery.rs`) — and the path it was handed is
            // our symlink, so the rename moves the LINK into the home's
            // db-backups and codex writes a fresh REAL file in its place. The
            // corrupt bytes stay in the store untouched, so without this the
            // healed DB dies with the session and every later one relinks to the
            // same corruption and recovers again, forever. A regular file where
            // this build placed a link is exactly that signal.
            for entry in CODEX_DURABLE_ENTRIES {
                let healed = self.home.join(entry);
                let Ok(meta) = healed.symlink_metadata() else {
                    continue;
                };
                if meta.file_type().is_symlink() || !meta.is_file() {
                    continue;
                }
                if let Err(e) = copy_file(&healed, &global.join(entry)) {
                    logline!("clauth: codex {entry} recovery sync-back failed: {e:#}");
                }
            }
        }

        if let Err(e) = with_state_lock(|_held| {
            if let Err(e) = crate::live_sessions::unregister(self.session.as_str()) {
                logline!("clauth: unregistering the live codex session failed: {e}");
            }
            if let Err(e) = std::fs::remove_file(&self.pid_file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                logline!("clauth: remove pid file failed: {e}");
            }
            let still_active = prune_stale_sessions(&self.sessions).unwrap_or(1);
            if still_active == 0 {
                // Never the bare stem: under Fake the shared home is the
                // profile's durable store, and removing it would be removing
                // the profile's memory because the last session left.
                if self.home.file_name().and_then(|n| n.to_str()) != Some(CODEX_HOME_STEM) {
                    let _ = std::fs::remove_dir_all(&self.home);
                }
                let _ = std::fs::remove_dir(&self.sessions);
            }
            Ok::<_, anyhow::Error>(())
        }) {
            logline!("clauth: codex session teardown failed: {e:#}");
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/runtime.rs"]
mod tests;
