//! Per-profile persistent `CLAUDE_CONFIG_DIR` used by `clauth start`.
//!
//! All `clauth start <profile>` sessions for the same profile share a
//! runtime tree at `~/.clauth/profiles/<profile>/runtime/`. Its
//! `.credentials.json` mirrors the profile's canonical creds so concurrent
//! sessions observe a single chain of refresh tokens. A watchdog thread in
//! each parent process keeps the runtime tree and canonical state in sync.
//!
//! Two transport modes, picked per profile at acquire time:
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
//! Reference counting lives in a sibling `sessions/` directory: each
//! session creates `sessions/<pid>-<n>` and holds an exclusive `flock(2)` on
//! it for its lifetime. The `-<n>` suffix keeps the file unique per acquire,
//! so one process holding several concurrent sessions of the same profile
//! (the `clauth mcp` server running overlapping `delegate`s) never collides
//! on a single path. New sessions prune entries whose lock is free (previous
//! holder died) and tear the runtime tree down when no live sessions remain.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::claude::{build_claude_settings_json, create_symlink};
use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::{
    ClaudeCredentials, Profile, atomic_write, atomic_write_600, claude_dir, clauth_dir, home_dir,
    profile_subpath,
};

/// Watchdog tick. 1s instead of a longer interval because fake-symlink mode
/// needs a tight upper bound on how long a session can read stale credentials
/// after a sibling refreshes — every additional second is another window in
/// which a 401 could revoke an already-rotated refresh token chain.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

/// `.claude.json` cross-profile sync cadence. Tighter than the credential
/// watchdog because Claude Code rewrites `.claude.json` constantly; 100ms keeps
/// the window in which one profile observes another's stale shared state small.
/// Also bounds watchdog-thread shutdown latency to one tick of this interval.
const CJSON_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    /// OS-level symlinks. Used on Unix unconditionally and on Windows when
    /// the process can create symlinks (developer mode or admin).
    Real,
    /// Bidirectional mtime-based mirror. Used on Windows when the OS denies
    /// symlink creation.
    Fake,
}

/// Whether a session inherits the operator's full `~/.claude/` (memory,
/// plugins, hooks, commands, agents) or runs authenticated-but-clean. An
/// isolated session gets its OWN `runtime-isolated/` + `sessions-isolated/`
/// trees so it never collides with a shared session of the same profile, while
/// sharing the profile's canonical credentials and rotation lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Isolation {
    /// Full mirror of `~/.claude/`: the session behaves like the operator's.
    Shared,
    /// Credentials injected, but operator memory/plugins/hooks/commands/agents
    /// omitted and settings built from an empty base — no house style leaks.
    Isolated,
}

impl Isolation {
    fn runtime_subdir(self) -> &'static str {
        match self {
            Isolation::Shared => "runtime",
            Isolation::Isolated => "runtime-isolated",
        }
    }
    fn sessions_subdir(self) -> &'static str {
        match self {
            Isolation::Shared => "sessions",
            Isolation::Isolated => "sessions-isolated",
        }
    }
}

/// The two runtime flavors a profile can hold concurrently. Liveness and GC
/// must consider both so a rotation never spends a token an isolated session
/// still holds.
const SESSION_ISOLATIONS: [Isolation; 2] = [Isolation::Shared, Isolation::Isolated];

/// Per-process counter making each `acquire`'s session file unique. A single
/// process can hold several live sessions of the same profile+flavor at once —
/// the `clauth mcp` server firing overlapping `delegate`s. Keying only on
/// `sessions/<pid>` would make the second acquire block forever on the first's
/// `flock(2)` (an exclusive lock on a second fd of the same path waits), hanging
/// the delegate in `acquire` with no session ever spawned. The suffix gives each
/// acquire its own liveness marker.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

fn runtime_dir(name: &str, isolation: Isolation) -> Result<PathBuf> {
    profile_subpath(name, isolation.runtime_subdir())
}

fn sessions_dir(name: &str, isolation: Isolation) -> Result<PathBuf> {
    profile_subpath(name, isolation.sessions_subdir())
}

fn profiles_root_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles"))
}

/// True iff the profile has at least one live `clauth start` session, of either
/// flavor (shared or isolated). Gates token rotation, so it MUST see an isolated
/// session too — otherwise a rotation could spend a refresh token the isolated
/// session still holds. A missing or unreadable sessions dir counts as idle.
pub(crate) fn has_live_session(name: &str) -> bool {
    SESSION_ISOLATIONS
        .iter()
        .any(|&iso| live_sessions_in(name, iso) > 0)
}

/// Count of live `clauth start` sessions for the profile across both flavors.
/// Additive sibling of [`has_live_session`]; a missing or unreadable sessions
/// dir counts as zero.
pub(crate) fn live_session_count(name: &str) -> usize {
    SESSION_ISOLATIONS
        .iter()
        .map(|&iso| live_sessions_in(name, iso))
        .sum()
}

/// Live-session count for one isolation flavor; zero when the dir is absent.
fn live_sessions_in(name: &str, isolation: Isolation) -> usize {
    let Ok(dir) = sessions_dir(name, isolation) else {
        return 0;
    };
    live_sessions_at(&dir)
}

/// Live sessions holding a marker in `sessions`; zero when the dir is absent.
/// Read-only (unlike [`prune_stale_sessions`], it drops nothing), so it needs no
/// state lock — a caller reading its own flavor's dir always counts ITSELF,
/// since a second fd's `try_lock` conflicts with the one `acquire` holds.
pub(crate) fn live_sessions_at(sessions: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| is_session_alive(&e.path()))
        .count()
}

/// The CDX-1b codex lease dir — `codex-sessions/<pid>-<n>` flock files, the
/// exact claude pattern in a harness-suffixed dir so flavors never collide.
pub(crate) fn codex_sessions_dir(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "codex-sessions")
}

/// True iff the profile has a live `clauth start` codex session. Its isolated
/// `CODEX_HOME` carries the profile's refresh chain while it runs, so this
/// gates the CDX-3 standby refresh, the switch/capture installers, and the
/// CDX-4 walk (PLAN.md §0.14 two-carrier refusals). Missing dir = idle.
pub(crate) fn has_live_codex_session(name: &str) -> bool {
    codex_sessions_dir(name)
        .map(|d| live_sessions_at(&d) > 0)
        .unwrap_or(false)
}

/// The profile's isolated `CODEX_HOME` tree (CDX-1b §0.14): `auth.json` =
/// the profile's chain (seeded from the store at acquire), `config.toml` a
/// COPY of the operator's, `sessions/` codex-created and isolated.
pub(crate) fn codex_home_dir(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "codex-home")
}

/// Codex adopt-back cadence. Far looser than the claude 1s watchdog: nothing
/// reads the isolated home but codex itself, and the only job here is bounding
/// how much chain rotation a SIGKILL could strand (the final Drop sync covers
/// clean exits).
const CODEX_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// One-directional adopt-back: isolated `codex-home/auth.json` → profile
/// store, when the bytes differ. The session process OWNS the store slot for
/// its profile while running (capture/switch refuse leased profiles), so this
/// is the only writer. Unparseable bytes are skipped (a half-written refresh
/// — the next tick sees the finished file); a missing file is a quiet no-op.
fn sync_codex_home_to_store(name: &str, codex_home: &Path) -> Result<()> {
    let path = codex_home.join("auth.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    if crate::codex::CodexAuthFile::parse(&bytes).is_err() {
        return Ok(());
    }
    if crate::codex::read_profile_auth(name)?.as_deref() == Some(&bytes[..]) {
        return Ok(());
    }
    crate::codex::write_profile_auth(name, &bytes)
}

/// Live-session guard for a codex profile — the CDX-1b sibling of
/// [`ProfileRuntime`], radically simpler (no symlinks, no LinkMode, no
/// settings merge): the tree is three entries and the watchdog is a one-way
/// adopt-back. On drop: final sync, pid-file release, teardown when last.
pub(crate) struct CodexRuntime {
    codex_home: PathBuf,
    pid_file: PathBuf,
    sessions: PathBuf,
    name: String,
    _pid_lock: File,
    watchdog_signal: Option<Sender<()>>,
    watchdog_handle: Option<JoinHandle<()>>,
}

impl CodexRuntime {
    /// Acquire a codex session runtime for `name` (a codex-harness profile —
    /// the caller dispatched on harness). Refusals implement the §0.14
    /// two-carrier rules; the `RotationGuard` window makes the stamp atomic
    /// against a concurrent standby refresh, exactly like the claude acquire.
    pub(crate) fn acquire(name: &str) -> Result<Self> {
        let stored = crate::codex::read_profile_auth(name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "profile '{name}' has no stored codex login — capture one with \
                 `clauth login {name} --codex` first"
            )
        })?;
        match crate::codex::store_mode() {
            mode if mode.is_file() => {}
            crate::codex::StoreMode::Other(mode) => anyhow::bail!(
                "codex stores credentials in '{mode}' mode (cli_auth_credentials_store) — an \
                 isolated CODEX_HOME would be ignored; clauth supports only the default 'file' \
                 mode"
            ),
            crate::codex::StoreMode::File => unreachable!("is_file() covered above"),
        }

        let codex_home = codex_home_dir(name)?;
        let sessions = codex_sessions_dir(name)?;
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid_file = sessions.join(format!("{}-{seq}", std::process::id()));

        // Same discipline as ProfileRuntime::acquire: the rotation guard is
        // held across the stamp window so a standby refresh can't spend this
        // chain mid-acquire; once the PID flock exists, the refresh's in-guard
        // lease re-check sees the session and skips.
        let _rotation_guard = RotationGuard::acquire(name)?;

        let pid_lock = with_state_lock(|| {
            // Two-carrier refusal, under the same lock a switch install holds:
            // if this profile's account IS the live login, codex in the shared
            // home already carries the chain — an isolated copy would fork it
            // (refresh_token_reused kill).
            let stored_id = crate::codex::CodexAuthFile::parse(&stored)
                .ok()
                .and_then(|a| a.account_id());
            if let (Some(stored_id), Ok(Some(live_bytes))) = (stored_id, crate::codex::read_live())
                && let Ok(live) = crate::codex::CodexAuthFile::parse(&live_bytes)
                && live.account_id().as_deref() == Some(stored_id.as_str())
            {
                anyhow::bail!(
                    "profile '{name}' is the LIVE codex login — its chain already runs in the \
                     shared ~/.codex; use plain `codex`, or switch the live login away first"
                );
            }

            std::fs::create_dir_all(&sessions)
                .with_context(|| format!("failed to create {}", sessions.display()))?;
            let active = prune_stale_sessions(&sessions)?;
            if active == 0 && codex_home.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&codex_home)
                    .with_context(|| format!("failed to clear {}", codex_home.display()))?;
            }
            std::fs::create_dir_all(&codex_home)
                .with_context(|| format!("failed to create {}", codex_home.display()))?;
            if active == 0 {
                atomic_write_600(&codex_home.join("auth.json"), &stored)
                    .context("failed to seed the isolated auth.json")?;
                // COPY (never symlink) the operator's config so model/trust
                // settings carry over while codex's own config writes stay
                // isolated (PLAN.md §0.14 — an in-place write through a
                // symlink would mutate the real ~/.codex/config.toml).
                let real_config = crate::codex::codex_dir()?.join("config.toml");
                if real_config.exists() {
                    std::fs::copy(&real_config, codex_home.join("config.toml"))
                        .context("failed to copy config.toml into the isolated home")?;
                }
            }
            let file = open_pid_file(&pid_file)
                .with_context(|| format!("failed to open {}", pid_file.display()))?;
            file.lock()
                .with_context(|| format!("failed to lock {}", pid_file.display()))?;
            Ok::<_, anyhow::Error>(file)
        })?;

        let (tx, rx) = channel::<()>();
        let watchdog_home = codex_home.clone();
        let watchdog_name = name.to_string();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let watchdog_handle = thread::Builder::new()
            .name(format!("clauth-cdx-wdog-{name}"))
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(CODEX_WATCHDOG_INTERVAL)
                {
                    if let Err(e) = sync_codex_home_to_store(&watchdog_name, &watchdog_home) {
                        logline!("clauth: codex watchdog sync failed: {e}");
                    }
                }
            })
            .expect("failed to spawn codex watchdog thread");

        Ok(Self {
            codex_home,
            pid_file,
            sessions,
            name: name.to_string(),
            _pid_lock: pid_lock,
            watchdog_signal: Some(tx),
            watchdog_handle: Some(watchdog_handle),
        })
    }

    pub(crate) fn codex_home(&self) -> &Path {
        &self.codex_home
    }
}

impl Drop for CodexRuntime {
    fn drop(&mut self) {
        drop(self.watchdog_signal.take());
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }
        // Final adopt-back: the session's last chain rotation reaches the
        // store even though the tree is about to be torn down.
        if let Err(e) = sync_codex_home_to_store(&self.name, &self.codex_home) {
            logline!("clauth: codex final sync failed: {e}");
        }
        if let Err(e) = with_state_lock(|| {
            if let Err(e) = std::fs::remove_file(&self.pid_file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                logline!("clauth: remove codex pid file failed: {e}");
            }
            let still_active = prune_stale_sessions(&self.sessions).unwrap_or(1);
            if still_active == 0 {
                let _ = std::fs::remove_dir_all(&self.codex_home);
                let _ = std::fs::remove_dir(&self.sessions);
            }
            Ok::<_, anyhow::Error>(())
        }) {
            logline!("clauth: codex drop cleanup failed: {e}");
        }
    }
}

/// Best-effort sweep removing runtime trees whose owning session died without
/// running teardown (SIGKILL/crash leaves `runtime/` + a stale `sessions/<pid>`).
/// Safe at any entry point: each removal re-checks liveness under the state lock
/// (the same teardown gate `Drop` uses), so a profile with a live session — or
/// one mid-acquire holding the lock — is never collected.
pub(crate) fn gc_stale_runtimes() {
    let Ok(root) = profiles_root_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        for iso in SESSION_ISOLATIONS {
            let _ = gc_one_runtime(&name, iso);
        }
        let _ = gc_one_codex_runtime(&name);
    }
}

/// Public entry to [`gc_one_codex_runtime`] for one profile — the standby
/// refresh calls this BEFORE spending a parked chain so a SIGKILLed isolated
/// session's fresher chain is adopted back into the store first (else the
/// store's stale token gets spent and reuse-killed; review MED). Best-effort:
/// a failure just leaves the tree for the next sweep.
pub(crate) fn gc_one_codex_runtime_public(name: &str) {
    if let Err(e) = gc_one_codex_runtime(name) {
        logline!("clauth: codex runtime adopt-back for '{name}' failed: {e}");
    }
}

/// CDX-1b flavor of [`gc_one_runtime`]: a `codex-home/` whose owning session
/// died without teardown. Adopt the stranded auth.json back FIRST — a SIGKILL
/// between two watchdog ticks may have left the freshest chain only here.
fn gc_one_codex_runtime(name: &str) -> Result<()> {
    let codex_home = codex_home_dir(name)?;
    if codex_home.symlink_metadata().is_err() {
        return Ok(());
    }
    let sessions = codex_sessions_dir(name)?;
    with_state_lock(|| {
        if prune_stale_sessions(&sessions).unwrap_or(0) == 0 {
            if let Err(e) = sync_codex_home_to_store(name, &codex_home) {
                logline!("clauth: codex gc adopt-back failed for '{name}': {e}");
            }
            let _ = std::fs::remove_dir_all(&codex_home);
            let _ = std::fs::remove_dir(&sessions);
        }
        Ok::<_, anyhow::Error>(())
    })
}

fn gc_one_runtime(name: &str, isolation: Isolation) -> Result<()> {
    let runtime = runtime_dir(name, isolation)?;
    if runtime.symlink_metadata().is_err() {
        return Ok(()); // nothing left behind for this flavor
    }
    let sessions = sessions_dir(name, isolation)?;
    with_state_lock(|| {
        if prune_stale_sessions(&sessions).unwrap_or(0) == 0 {
            let _ = std::fs::remove_dir_all(&runtime);
            let _ = std::fs::remove_dir(&sessions);
        }
        Ok::<_, anyhow::Error>(())
    })
}

/// Every profile with a live isolated session, paired with its own
/// `runtime-isolated/projects/` dir. An isolated runtime's transcripts live
/// ONLY in this throwaway tree (never symlinked to the global store) and are
/// discarded on teardown/GC, so the session index can reach them only while the
/// session is live. Gated on a live *isolated* session specifically (not
/// [`has_live_session`], which also counts shared sessions) and on the projects
/// dir existing, so a shared-only or not-yet-written runtime is skipped.
/// Fail-soft: an unreadable profiles root or entry is skipped, never an error.
pub(crate) fn live_isolated_stores() -> Vec<(String, PathBuf)> {
    let Ok(root) = profiles_root_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if live_sessions_in(&name, Isolation::Isolated) == 0 {
            continue;
        }
        let Ok(projects) = runtime_dir(&name, Isolation::Isolated).map(|d| d.join("projects"))
        else {
            continue;
        };
        if projects.is_dir() {
            out.push((name, projects));
        }
    }
    out
}

fn canonical_credentials(name: &str) -> Result<PathBuf> {
    // CLA-FEED: arm a feed profile's sidecar BEFORE resolving the source —
    // a session launched inside an arming window (flag on, sidecar not yet
    // fed) would otherwise copy the rotating pair, and the daemon's later
    // rotations (feed-exempted from the live-session bail only for ARMED
    // sidecars) could still race a hand-armed state. Best-effort by design.
    crate::claude::arm_feed_from_disk(name);
    // CLA-SPLIT: a `clauth start` session runs on what a switch would install —
    // the static session token when the profile has one. The rotating usage
    // pair in `credentials.json` must never be handed to a session (it would
    // re-arm the session-vs-refresher single-use-chain race the split removes).
    crate::claude::install_source_path(name)
}

fn rotation_lock_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "rotation.lock")
}

/// Cross-process advisory lock serializing a token rotation against a
/// `clauth start` session acquire for the SAME profile.
///
/// A refresh token is single-use: once `oauth::refresh` spends it the server
/// kills it, and a second refresh of the same token 401s and burns the whole
/// chain. The global state flock (`with_state_lock`) cannot guard this because
/// it must be released across the network round trip; the per-PID session
/// flocks only track liveness, not "a rotation is in flight". This lock is
/// held for the FULL rotate HTTP window (which `with_state_lock` cannot be),
/// and `ProfileRuntime::acquire` takes the same lock before it stamps its
/// session PID file — so the two operations are mutually exclusive:
///
/// - rotate wins the race → acquire blocks until the new pair is persisted,
///   then the session starts against the rotated token;
/// - acquire wins the race → it creates its session PID file before releasing,
///   so rotate's in-lock `has_live_session` re-check sees the live session and
///   skips (the token is never spent).
///
/// Distinct from `~/.clauth/.lock` (global state) and `sessions/<pid>`
/// (per-session liveness). Blocking `flock`; the holder window is short.
#[must_use]
pub(crate) struct RotationGuard {
    // Drops before `_rank` (declaration order): the flock releases, then the
    // ROTATION rank pops — never the reverse.
    _file: File,
    _rank: crate::lockorder::RankGuard,
}

impl RotationGuard {
    /// Acquire the per-profile rotation lock, blocking until any in-flight
    /// rotation or acquire for this profile releases it. Creates the directory
    /// if missing (a profile with no session yet has no `sessions/`).
    pub(crate) fn acquire(name: &str) -> Result<Self> {
        let path = rotation_lock_path(name)?;
        if let Some(parent) = path.parent() {
            crate::profile::mkdir_700(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file =
            open_pid_file(&path).with_context(|| format!("failed to open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        // ROTATION is the outermost rank — held across the OAuth HTTP round
        // trip, before `config` and the state flock are ever taken.
        let _rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::Rotation>();
        Ok(Self { _file: file, _rank })
    }
}

/// Non-blocking probe of the per-profile rotation lock (CDX-3 §0.9). The
/// codex switch/capture installers run under the state flock, where a
/// blocking [`RotationGuard::acquire`] would invert the Rotation-outermost
/// rank — but a try-lock can never block, so it cannot participate in a
/// deadlock cycle, and it deliberately carries NO `RankGuard` (the rank
/// system exists to prevent blocking cycles; a failed probe simply reports
/// "a standby refresh holds this chain right now"). Holding the returned
/// probe keeps a standby refresh of the same profile blocked (its blocking
/// acquire waits) until the install window closes.
#[must_use]
pub(crate) struct RotationProbe {
    _file: File,
}

impl RotationProbe {
    /// `Ok(None)` when another holder (standby refresh, session acquire, or
    /// claude rotation) currently owns the profile's rotation lock.
    pub(crate) fn try_acquire(name: &str) -> Result<Option<Self>> {
        let path = rotation_lock_path(name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file =
            open_pid_file(&path).with_context(|| format!("failed to open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(anyhow::Error::from(e).context(format!("failed to probe {}", path.display())))
            }
        }
    }
}

/// Open or create a PID file without truncating — used for session liveness
/// tracking via flock. `O_CREAT` without truncate preserves any existing lock
/// held by a sibling that raced us to create the file. Owner-only (0o600) via
/// [`crate::profile::open_state_file`], the shared opener for every `~/.clauth`
/// lock (this also covers `rotation.lock`, opened through here).
pub(crate) fn open_pid_file(path: &Path) -> std::io::Result<File> {
    crate::profile::open_state_file(path)
}

/// Live-session guard. On drop: stops the watchdog, runs a final sync
/// (errors surface to stderr), drops the PID file, and tears the runtime
/// down when this was the last session for the profile.
pub(crate) struct ProfileRuntime {
    runtime: PathBuf,
    pid_file: PathBuf,
    claude_home: PathBuf,
    canonical: PathBuf,
    sessions: PathBuf,
    mode: LinkMode,
    isolation: Isolation,
    /// Held for the lifetime of the session so a sibling process's
    /// `try_lock` reveals we're still alive.
    _pid_lock: File,
    /// Wrapped in Option so Drop can take() it before joining the watchdog,
    /// signalling the thread to exit.
    watchdog_signal: Option<Sender<()>>,
    watchdog_handle: Option<JoinHandle<()>>,
}

impl ProfileRuntime {
    pub(crate) fn acquire(
        profile: &Profile,
        isolation: Isolation,
        active_env_keys: &[String],
    ) -> Result<Self> {
        let name = &profile.name;
        let claude_home = claude_dir()?;
        if !claude_home.exists() {
            anyhow::bail!("~/.claude not found; install Claude Code first");
        }
        let runtime = runtime_dir(name, isolation)?;
        let sessions = sessions_dir(name, isolation)?;
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid_file = sessions.join(format!("{}-{seq}", std::process::id()));
        let canonical = canonical_credentials(name)?;

        // Hold the per-profile rotation lock across the session-stamp window so
        // a concurrent `oauth::rotate_one_inner` for this profile cannot spend the
        // single-use refresh token while we are starting up. Ordering rule
        // (matches `oauth::rotate_one_inner`): RotationGuard OUTERMOST, then the
        // state flock inside. Dropped right after the PID file is locked — from
        // then on the PID flock itself signals liveness, and rotate's in-lock
        // `has_live_session` re-check observes it.
        let _rotation_guard = RotationGuard::acquire(name)?;

        let (pid_lock, mode) = with_state_lock(|| {
            crate::profile::mkdir_700(&sessions)
                .with_context(|| format!("failed to create {}", sessions.display()))?;
            let active = prune_stale_sessions(&sessions)?;
            // No live siblings — rebuild from scratch so stale symlinks/copies
            // to entries that have since vanished from ~/.claude/ don't carry over.
            if active == 0 && runtime.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&runtime)
                    .with_context(|| format!("failed to clear {}", runtime.display()))?;
            }
            crate::profile::mkdir_700(&runtime)
                .with_context(|| format!("failed to create {}", runtime.display()))?;
            let mode = detect_link_mode(&runtime)?;
            build_runtime_dir_with_active_env(
                &runtime,
                &claude_home,
                profile,
                &canonical,
                mode,
                isolation,
                active_env_keys,
            )?;
            let file = open_pid_file(&pid_file)
                .with_context(|| format!("failed to open {}", pid_file.display()))?;
            file.lock()
                .with_context(|| format!("failed to lock {}", pid_file.display()))?;
            Ok::<_, anyhow::Error>((file, mode))
        })?;

        let (tx, rx) = channel::<()>();
        let watchdog_runtime = runtime.clone();
        let watchdog_canonical = canonical.clone();
        let watchdog_claude_home = claude_home.clone();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let watchdog_handle = thread::Builder::new()
            .name(format!("clauth-wdog-{name}"))
            .spawn(move || {
                // One thread, two cadences: the config reconcilers
                // (`.claude.json` + `settings.json`) run every CJSON_INTERVAL;
                // credentials reconcile every ~WATCHDOG_INTERVAL, counted in
                // cjson ticks. Loop exits on Disconnected (sender dropped in
                // Drop) or Ok(()).
                let cred_every =
                    (WATCHDOG_INTERVAL.as_millis() / CJSON_INTERVAL.as_millis()).max(1);
                let mut until_cred = cred_every;
                while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(CJSON_INTERVAL) {
                    if let Err(e) = crate::claude_json::sync_once() {
                        logline!("clauth: .claude.json sync failed: {e}");
                    }
                    if let Err(e) = crate::settings_sync::sync_once() {
                        logline!("clauth: settings.json sync failed: {e}");
                    }
                    until_cred -= 1;
                    if until_cred == 0 {
                        until_cred = cred_every;
                        if let Err(e) = tick(
                            mode,
                            isolation,
                            &watchdog_runtime,
                            &watchdog_claude_home,
                            &watchdog_canonical,
                        ) {
                            logline!("clauth: watchdog tick failed: {e}");
                        }
                    }
                }
            })
            .expect("failed to spawn watchdog thread");

        Ok(Self {
            runtime,
            pid_file,
            claude_home,
            canonical,
            sessions,
            mode,
            isolation,
            _pid_lock: pid_lock,
            watchdog_signal: Some(tx),
            watchdog_handle: Some(watchdog_handle),
        })
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.runtime
    }

    /// This session's liveness-marker dir. Its live count gates anything that
    /// MOVES state out of `config_dir`: the runtime tree is shared by every
    /// session of this profile+flavor (`runtime_dir` carries no pid), and only
    /// the last one out sees it discarded.
    pub(crate) fn sessions_dir(&self) -> &Path {
        &self.sessions
    }
}

impl Drop for ProfileRuntime {
    fn drop(&mut self) {
        // Drop the sender to signal the watchdog, then join.
        drop(self.watchdog_signal.take());
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }

        if let Err(e) = tick(
            self.mode,
            self.isolation,
            &self.runtime,
            &self.claude_home,
            &self.canonical,
        ) {
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

        if let Err(e) = with_state_lock(|| {
            if let Err(e) = std::fs::remove_file(&self.pid_file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                logline!("clauth: remove pid file failed: {e}");
            }
            let still_active = prune_stale_sessions(&self.sessions).unwrap_or(1);
            if still_active == 0 {
                let _ = std::fs::remove_dir_all(&self.runtime);
                let _ = std::fs::remove_dir(&self.sessions);
            }
            Ok::<_, anyhow::Error>(())
        }) {
            logline!("clauth: drop cleanup failed: {e}");
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
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

/// Drop [`MANAGED_ENV_KEYS`] plus the active profile's custom env keys from
/// `command`'s inherited env, so the target's runtime `settings.json` is the
/// sole source for them. Shared by `clauth start` and the MCP delegate. Call
/// before layering any caller-supplied env, so a caller can still set a key
/// back deliberately.
pub(crate) fn scrub_profile_env(command: &mut std::process::Command, active_env_keys: &[String]) {
    for key in MANAGED_ENV_KEYS {
        command.env_remove(key);
    }
    for key in active_env_keys {
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

/// Probe the OS by attempting a real symlink in the runtime root. Anything
/// other than success — privilege denial, unsupported filesystem, the
/// `cfg(not(any(unix, windows)))` fallback — drops to fake-symlink mode.
fn detect_link_mode(runtime: &Path) -> Result<LinkMode> {
    let probe_target = runtime.join(".clauth-probe-target");
    let probe_link = runtime.join(".clauth-probe-link");
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
fn prune_stale_sessions(sessions: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(sessions) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let mut alive = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if is_session_alive(&path) {
            alive += 1;
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(alive)
}

fn is_session_alive(pid_file: &Path) -> bool {
    // Open without O_CREAT: creating the file would race with another session
    // that just created it but hasn't locked it yet, producing a false
    // "unlocked = dead" reading. try_lock succeeds iff no other open fd holds
    // an exclusive flock, i.e. the previous owner has exited.
    let Ok(file) = OpenOptions::new().read(true).write(true).open(pid_file) else {
        return false;
    };
    // Any I/O error: treat as alive so we don't race a live session.
    file.try_lock().is_err()
}

/// Build or incrementally update the runtime tree.
///
/// Called on every `acquire`, including when `active > 0` (siblings already
/// built the tree). The walk always runs; entries whose runtime counterpart
/// already exists are skipped, so `~/.claude/` additions after the first build
/// are picked up without disturbing the rest.
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
/// `active_env_keys` (the live-active profile's custom env) are stripped from
/// the shared `settings.json` base before this profile's overrides are merged,
/// so a `clauth start <other>` session does not inherit the active profile's
/// custom `[env]`. Model + endpoint keys are re-derived per profile in
/// `build_claude_settings_json`, so only custom `[env]` needs this strip.
fn build_runtime_dir_with_active_env(
    runtime: &Path,
    claude_home: &Path,
    profile: &Profile,
    canonical: &Path,
    mode: LinkMode,
    isolation: Isolation,
    active_env_keys: &[String],
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
    write_merged_settings(runtime, claude_home, profile, isolation, active_env_keys)?;

    let creds_link = runtime.join(".credentials.json");
    reconcile_credentials(&creds_link, canonical, mode)?;

    seed_claude_json(runtime, claude_home)?;

    Ok(())
}

/// Test-only convenience over [`build_runtime_dir_with_active_env`]: no active
/// profile, so nothing is stripped from the inherited base. Inline runtime
/// tests build dirs directly without a live active profile in scope.
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
/// and directories are never removed. `.credentials.json` is reconciled
/// separately afterwards, so pruning a stale one here is safe.
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
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Compute this profile's merged `settings.json` and write it into the runtime
/// tree only when absent or byte-different. Concurrent sessions on the same
/// profile each compute the same merge, so a byte-identical result needn't win
/// a last-writer race and stomp a sibling's write. Isolated mode builds from an
/// empty base (no operator hooks/permissions/statusline/plugin config), keeping
/// only the profile's own env + model routing.
///
/// `active_env_keys` (the live-active profile's custom env) are stripped from
/// the shared base first, so a `clauth start <other>` session does not inherit
/// the active profile's custom `[env]`. Model + endpoint keys are re-derived
/// per profile in `build_claude_settings_json`, so only custom `[env]` needs
/// this. Callers with no active profile pass empty; starting the active profile
/// itself passes its own keys, which the merge re-inserts (a no-op strip).
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
    active_env_keys: &[String],
) -> Result<()> {
    let settings_src = claude_home.join("settings.json");
    let base = match isolation {
        Isolation::Shared => Some(settings_src.as_path()),
        Isolation::Isolated => None,
    };
    let merged = build_claude_settings_json(base, profile, active_env_keys)?;
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
        // TECH-9 #16: runtime settings.json can embed a third-party api_key — 0o600.
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
    let meta = src
        .symlink_metadata()
        .with_context(|| format!("failed to stat {}", src.display()))?;
    if meta.file_type().is_dir() {
        std::fs::create_dir_all(dst)
            .with_context(|| format!("failed to create {}", dst.display()))?;
        for entry in
            std::fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))?
        {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst)
            .map(|_| ())
            .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))
    }
}

/// One watchdog iteration. Real mode only repairs `.credentials.json` (the rest
/// is symlinks needing no maintenance). Fake mode reconciles every tree file by
/// mtime, plus the credentials file — except in isolated mode, where the tree
/// mirror is skipped so it never re-seeds the operator memory/plugins the
/// isolated runtime deliberately omits (`mirror_tree` is additive and would
/// copy `~/.claude/CLAUDE.md` back in). Credentials still reconcile.
fn tick(
    mode: LinkMode,
    isolation: Isolation,
    runtime: &Path,
    claude_home: &Path,
    canonical: &Path,
) -> Result<()> {
    match mode {
        LinkMode::Real => {
            let _ = sync_credentials(runtime, canonical)?;
            Ok(())
        }
        LinkMode::Fake if isolation == Isolation::Isolated => {
            with_state_lock(|| mirror_credentials(&runtime.join(".credentials.json"), canonical))
        }
        LinkMode::Fake => {
            // Bulk tree walk + copies run WITHOUT the state lock: on a large
            // ~/.claude/ holding the lock across the walk stalled every
            // concurrent acquire / CLI switch for hundreds of ms per tick.
            // Lockless-safe: every per-file merge is independent, self-converging
            // under "latest mtime wins" + byte-equality skip, and never deletes
            // — a file changing in the TOCTOU window re-converges next tick.
            // mirror_tree skips settings.json / .credentials.json, so it never
            // races build_runtime_dir's per-profile writes. Only credential
            // reconciliation (must not interleave with acquire/switch credential
            // writes) stays under the lock.
            mirror_tree(claude_home, runtime)?;
            with_state_lock(|| mirror_credentials(&runtime.join(".credentials.json"), canonical))
        }
    }
}

/// If Claude Code's internal refresh replaced `<runtime>/.credentials.json` with
/// a regular file, copy its bytes into canonical creds and swap the file back to
/// a symlink so canonical stays the single source of truth. Returns `true` when
/// bytes were written. Real-symlink mode only — fake mode uses
/// [`mirror_credentials`].
pub(crate) fn sync_credentials(runtime: &Path, canonical: &Path) -> Result<bool> {
    let link_path = runtime.join(".credentials.json");
    with_state_lock(|| sync_credentials_unlocked(&link_path, canonical))
}

fn sync_credentials_unlocked(link_path: &Path, canonical: &Path) -> Result<bool> {
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
        let canonical_mtime = std::fs::metadata(canonical)
            .ok()
            .and_then(|m| m.modified().ok());
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
fn mirror_tree(claude_home: &Path, runtime: &Path) -> Result<()> {
    // `.claude.json` is a per-profile copy reconciled by `crate::claude_json`,
    // not part of the `~/.claude/` tree — skip it here so the tree mirror never
    // copies it into `~/.claude/.claude.json`.
    let skip_top: HashSet<&str> = ["settings.json", ".credentials.json", ".claude.json"]
        .into_iter()
        .collect();
    for name in union_children(claude_home, runtime) {
        if name.to_str().is_some_and(|n| skip_top.contains(n)) {
            continue;
        }
        merge_path(&claude_home.join(&name), &runtime.join(&name))?;
    }
    Ok(())
}

/// Unioned child-name set of two directories. Absent/unreadable side
/// contributes nothing. Names sorted for deterministic, stable iteration.
fn union_children(a: &Path, b: &Path) -> Vec<std::ffi::OsString> {
    let mut names: HashSet<std::ffi::OsString> = HashSet::new();
    for dir in [a, b] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                names.insert(entry.file_name());
            }
        }
    }
    let mut out: Vec<_> = names.into_iter().collect();
    out.sort();
    out
}

/// Reconcile one path between canonical (`a`) and runtime (`b`) sides.
/// Directories recurse via the same union-walk; files merge by mtime.
fn merge_path(a: &Path, b: &Path) -> Result<()> {
    let a_meta = a.symlink_metadata().ok();
    let b_meta = b.symlink_metadata().ok();

    let a_is_dir = a_meta.as_ref().is_some_and(|m| m.file_type().is_dir());
    let b_is_dir = b_meta.as_ref().is_some_and(|m| m.file_type().is_dir());

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
            merge_path(&a.join(&name), &b.join(&name))?;
        }
        return Ok(());
    }

    match (a_meta, b_meta) {
        (Some(am), Some(bm)) => {
            let a_time = am.modified().ok();
            let b_time = bm.modified().ok();
            if files_match(a, b)? {
                return Ok(());
            }
            if mtime_newer(a_time, b_time) {
                copy_file(a, b)?;
            } else if mtime_newer(b_time, a_time) {
                copy_file(b, a)?;
            }
        }
        (Some(_), None) => {
            copy_file(a, b)?;
        }
        (None, Some(_)) => {
            copy_file(b, a)?;
        }
        (None, None) => {}
    }
    Ok(())
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

/// Copy `src` onto `dst` via a PID-suffixed tmp + atomic rename. `mirror_tree`
/// runs lockless, so a concurrent reader (sibling session, user, or
/// `build_runtime_dir`) could observe `dst` mid-write; a raw `std::fs::copy`
/// truncates-then-streams (non-atomic, seen torn). The rename makes the swap
/// atomic on POSIX (observer sees old or complete-new); the PID suffix keeps two
/// processes off the same tmp name.
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    atomic_write(dst, &bytes)
        .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))
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
