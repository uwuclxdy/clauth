//! `clauth start <name>` — spawn `claude` against this session's own runtime
//! directory. See [`crate::runtime`] for the per-session runtime design; this
//! module is just the thin wrapper that owns the lifetime guard.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
#[cfg(unix)]
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::Duration;
use std::time::SystemTime;

use anyhow::{Context, Result};
#[cfg(unix)]
use signal_hook::consts::signal::{SIGINT, SIGTERM};
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use crate::logline::logline;
use crate::profile::{AppConfig, ProfileName};
use crate::runtime::{Isolation, ProfileRuntime};
use crate::spinner::Spinner;

#[cfg(unix)]
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(50);

struct ChildOutcome {
    status: ExitStatus,
    signal: Option<i32>,
}

/// Lift an exiting isolated session's state into the global store: the
/// transcripts under `projects/`, then Claude Code's own session sidecar state
/// (shell snapshots, file history, tasks/plans, …) from the rest of the runtime
/// root, so a rescued session keeps more than its resumability. Returns
/// `(transcripts, sidecar files)` moved. Best-effort throughout: an error is
/// logged, never fails the run.
///
/// Gated on being the only live marker in `sessions`, because the count — not
/// the keying — is what proves nothing is reading the tree being emptied: the
/// sidecar leg would otherwise pull `shell-snapshots/` out from under a live
/// Claude Code mid-session. Self holds its own marker, hence `> 1`.
///
/// Under real symlinks each session owns its tree and marker dir, so the count is
/// this session alone and the guard never fires. It DOES fire on a fake-symlink
/// host, where the profile's isolated sessions share one tree: the first out
/// rescues nothing and the last out rescues everything, since the shared tree
/// holds every session's transcripts. The consequence is that the rescue becomes
/// all-or-nothing on the last session's clean exit — SIGKILL the last one and GC
/// discards the tree with every session's transcripts in it. Not separable while
/// the tree is shared: the sidecar trees carry no per-session attribution.
pub(crate) fn rescue_teardown(
    iso_root: &Path,
    sessions: &Path,
    claude_home: &Path,
) -> (usize, usize) {
    // An unreadable marker dir falls to "do not move": this leg pulls
    // `shell-snapshots/` out from under whatever is reading the tree, so an
    // unknown has to read the same way a live sibling does.
    if crate::runtime::live_sessions_at(sessions).is_none_or(|live| live > 1) {
        logline!("clauth: skipping rescue, another isolated session is still live");
        return (0, 0);
    }
    let moved = crate::sessions::rescue_isolated_store(
        &iso_root.join("projects"),
        &claude_home.join("projects"),
    );
    let sidecars = crate::sessions::rescue_isolated_sidecars(iso_root, claude_home);
    if moved > 0 || sidecars > 0 {
        logline!(
            "clauth: rescued {moved} isolated session transcript(s) \
             + {sidecars} sidecar file(s) into the global store"
        );
    }
    (moved, sidecars)
}

/// The refusal a `--with-fallback` start gets on a host that structurally cannot
/// execute a per-session credential swap. Split from the gate so BOTH causes are
/// exercised from a Linux run: `cfg!(target_os = "macos")` and [`LinkMode::Fake`]
/// are each unreachable there.
fn unsupported_host_refusal(name: &ProfileName, why: crate::runtime::SwapUnsupported) -> String {
    format!(
        "'{name}': --with-fallback needs a per-session credential swap, but {why}; start without it"
    )
}

/// Every reason `--with-fallback` cannot be honored for `name`, refused before
/// `acquire` builds a tree and long before `claude` is spawned. A flag that
/// silently leaves the session on its launch account is the one outcome the live
/// Claude Code probe exists to prevent, so none of these is a warning.
///
/// Every gate that can answer WITHOUT the disk runs first, in unfixable-first
/// order, and the transport probe runs last. That ordering is load-bearing twice
/// over: a start refused for a cause the user can act on never materializes a
/// profile dir for an account that never launched, and the compile-time macOS
/// verdict never arrives as a state-lock timeout or an IO error from a probe it
/// did not need. `is_macos` is the caller's `cfg!`, so the keychain arm is
/// testable off a Mac.
fn refuse_unless_chain_eligible(
    config: &AppConfig,
    profile: &crate::profile::Profile,
    isolation: Isolation,
    is_macos: bool,
) -> Result<()> {
    let name = &profile.name;
    // clap already refuses the flag pair, so this is for a caller that bypasses
    // it: `chain_opt_in_survives` drops an isolated opt-in silently, which is the
    // one outcome every gate here exists to prevent.
    if isolation == Isolation::Isolated {
        anyhow::bail!(
            "'{name}': --with-fallback cannot be combined with --isolated, since an \
             isolated session follows no chain"
        );
    }
    if let Some(why) = crate::runtime::unsupported_swap_platform(is_macos) {
        anyhow::bail!("{}", unsupported_host_refusal(name, why));
    }
    // The decision leg's freshness gate reads only the OAuth status store. That
    // is sound because a third-party-launched session gets a chain the walk
    // cannot move it off — so an opted-in one would follow nothing, in silence.
    if !profile.is_oauth() {
        anyhow::bail!(
            "'{name}': --with-fallback needs an OAuth account, but this one carries \
             a custom endpoint; start without it"
        );
    }
    // `snapshot_session_chain` returns `None` for a member outside the chain, so
    // the row is skipped every tick with nothing said.
    if !config.state.fallback_chain.iter().any(|n| n == name) {
        anyhow::bail!(
            "'{name}': --with-fallback needs a fallback-chain member; add '{name}' on \
             the fallback tab, or start without it"
        );
    }
    // Membership alone is not enough: a chain holding only this profile gives
    // `next_auto_switch_target` nowhere to point, and both `Off` and a stay-put
    // `None` write nothing on the session path. Same silence as a non-member.
    if !config.state.fallback_chain.iter().any(|n| n != name) {
        anyhow::bail!(
            "'{name}': --with-fallback needs a second account in the fallback chain to \
             move to; add one on the fallback tab, or start without it"
        );
    }
    // Only the daemon's decision leg writes `intended_member`, so with no daemon
    // the flag is inert. `singleton_held` is the decision-side reader: it
    // separates "nobody there" from "can't tell", and a host that cannot be
    // checked cannot run the decider on it either, so both refuse.
    let held = crate::daemon::singleton_held().with_context(|| {
        format!("'{name}': --with-fallback needs a running daemon and this host could not be checked for one")
    })?;
    if !held {
        anyhow::bail!(
            "'{name}': --with-fallback needs a running daemon to decide switches, \
             run `clauth daemon`"
        );
    }
    // Last, because it is the only gate that touches disk.
    if let Some(why) = crate::runtime::unsupported_swap_transport(name)? {
        anyhow::bail!("{}", unsupported_host_refusal(name, why));
    }
    Ok(())
}

pub(crate) fn run(
    config: &AppConfig,
    name: &ProfileName,
    claude_args: &[String],
    isolation: Isolation,
    workspace: Option<&Path>,
    follows_chain: bool,
) -> Result<()> {
    // Authoritative "never a live session for a disabled account" gate — every
    // caller (`cmd_start`, `sessions_cli::run_resume`) inherits it here, before
    // any side effect (runtime acquire, spawn). A wrapper's own pre-check is a
    // friendly early error at best; this one can't be bypassed by adding a new
    // caller that forgets to check.
    crate::refuse_if_disabled(config, name)?;
    let profile = config.find(name).context("profile not found")?;
    if follows_chain {
        refuse_unless_chain_eligible(config, profile, isolation, cfg!(target_os = "macos"))?;
    }

    // The plugin-migration pre-flight: heal a broken or divergent clauth
    // marketplace registration before the session launches, so the session loads
    // its hooks and MCP. A healthy registration costs this nothing — the gate is
    // two registry-file reads and spawns no `claude`. Best-effort: a failed heal
    // is logged, never fails the start. `claude` the binary (not just the plugin
    // CLI) needs to be on PATH anyway for the spawn below to succeed, and an
    // uninstalled plugin heals to a one-read no-op.
    crate::plugin_host::preflight();

    // Strip the active profile's custom env from the inherited base so a
    // `clauth start <other>` session doesn't inherit it. The live
    // `settings.json` is owned by whoever is active; starting that same profile
    // passes its own keys, which the merge re-inserts (no-op).
    let active_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_ref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing runtime");
        ProfileRuntime::acquire(profile, isolation, &active_env_keys, follows_chain)?
    };

    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    // Through the runtime-spawn seam: this is a claude session, and the codex
    // engine plugs into the same three calls when its runtime lands.
    let engine: &dyn crate::harness::HarnessEngine = &crate::harness::ClaudeEngine;
    let mut command = engine.command();
    // Scrub clauth-managed + active custom env so a session started under
    // profile B doesn't inherit profile A's endpoint/auth/model overrides from
    // the parent process env. The target's runtime settings.json re-supplies
    // whichever it defines. Mirrors the delegate path (run_delegate).
    engine.scrub_env(&mut command, &active_env_keys);
    // A resume pins `claude` to the session's workspace; a normal start inherits
    // this process's cwd. Either way the resolved dir feeds the home-project
    // settings guard: when it is the real `$HOME`, its project-tier settings
    // lookup would hit the real `~/.claude/settings.json` and re-leak the
    // globally active profile's env, outranking the runtime settings.json below.
    let spawn_cwd = apply_spawn_cwd(&mut command, workspace);
    if let Some(cwd) = spawn_cwd.as_deref() {
        crate::runtime::guard_home_project_settings(&mut command, cwd);
    }
    command.env(engine.home_env_key(), runtime.config_dir());
    // Isolated: also suppress global/project MCP servers wired through
    // `.claude.json`, so the only extension surface is what the caller passes.
    // Deliberately NOT `--safe-mode`. The cross-account leak (the operator's
    // `~/.claude/plugins`) is already gone under the empty config dir. What
    // remains is a cwd `.claude/skills/*` plugin: project-local and trust-gated,
    // loading the same regardless of active account (like project CLAUDE.md).
    // `--safe-mode` would also nuke cwd CLAUDE.md + skills, so it stays off.
    if isolation == Isolation::Isolated {
        command.arg("--strict-mcp-config");
    }
    // Marks this run's window: on the shared global store, only sessions touched
    // at or after this instant are attributed to `name` (see stamp below).
    let run_start = SystemTime::now();
    let mut child = command
        .args(claude_args)
        .spawn()
        .context("failed to spawn claude")?;

    #[cfg(unix)]
    let outcome = wait_for_child(&mut child, signal_watcher.receiver())?;

    #[cfg(not(unix))]
    let outcome = ChildOutcome {
        status: child
            .wait()
            .context("failed to wait for the session child")?,
        signal: None,
    };

    // Record which sessions ran under this profile before teardown — an isolated
    // store is discarded on drop, so its stamp must happen while `runtime` lives.
    // Isolated: the store is exclusive, so every transcript maps to `name`.
    // Shared: transcripts land in the global store, so only this run's window is.
    // Best-effort; never fails the completed session.
    let isolated = isolation == Isolation::Isolated;
    let projects_dir = if isolated {
        Some(runtime.config_dir().join("projects"))
    } else {
        crate::profile::claude_dir()
            .ok()
            .map(|d| d.join("projects"))
    };
    if let Some(projects_dir) = projects_dir {
        crate::sessions::stamp_run_sessions(name, &projects_dir, isolated, run_start);
    }

    // Rescue (isolated only): the throwaway isolated store is discarded on
    // `drop(runtime)`, taking the session's state with it, so lift it into the
    // global store first. A shared start needs nothing here — its transcripts
    // already live in the global store.
    if isolated && let Ok(claude_home) = crate::profile::claude_dir() {
        rescue_teardown(runtime.config_dir(), runtime.sessions_dir(), &claude_home);
    }

    // Drop runtime before process::exit so final sync + refcount cleanup runs.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Resolve the directory the spawned `claude` runs in and pin `command` to it.
/// `Some(dir)` sets the child's cwd to that workspace (a resume); `None` leaves
/// `command` inheriting this process's cwd (a normal start), so the `None` path
/// is byte-for-byte the pre-resume behavior. Returns the resolved dir so the
/// caller feeds the same path to the home-project settings guard, whose lookup
/// is cwd-based.
fn apply_spawn_cwd(
    command: &mut std::process::Command,
    workspace: Option<&Path>,
) -> Option<PathBuf> {
    match workspace {
        Some(dir) => {
            command.current_dir(dir);
            Some(dir.to_path_buf())
        }
        None => std::env::current_dir().ok(),
    }
}

fn status_code(status: ExitStatus, signal: Option<i32>) -> i32 {
    if status.success() {
        return signal.map_or(0, |s| 128 + s);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    thread: Option<JoinHandle<()>>,
    rx: Receiver<i32>,
}

#[cfg(unix)]
impl SignalWatcher {
    fn new() -> Result<Self> {
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).context("failed to install signal handlers")?;
        let handle = signals.handle();
        let (tx, rx) = channel();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        let thread = std::thread::Builder::new()
            .name("clauth-sig".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if tx.send(signal).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn signal watcher thread");
        Ok(Self {
            handle,
            thread: Some(thread),
            rx,
        })
    }

    fn receiver(&self) -> &Receiver<i32> {
        &self.rx
    }
}

#[cfg(unix)]
impl Drop for SignalWatcher {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
fn wait_for_child(
    child: &mut std::process::Child,
    signals: &Receiver<i32>,
) -> Result<ChildOutcome> {
    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to wait for the session child")?
        {
            return Ok(ChildOutcome {
                status,
                signal: next_signal(signals),
            });
        }

        match signals.recv_timeout(CHILD_WAIT_INTERVAL) {
            Ok(signal) => {
                forward_signal_or_warn(child, signal);
                return wait_after_signal(child, signals, signal);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(CHILD_WAIT_INTERVAL),
        }
    }
}

#[cfg(unix)]
fn wait_after_signal(
    child: &mut std::process::Child,
    signals: &Receiver<i32>,
    first_signal: i32,
) -> Result<ChildOutcome> {
    let mut signal = first_signal;
    loop {
        match child
            .try_wait()
            .context("failed to wait for the session child")?
        {
            Some(status) => {
                return Ok(ChildOutcome {
                    status,
                    signal: Some(signal),
                });
            }
            None => match signals.recv_timeout(CHILD_WAIT_INTERVAL) {
                Ok(next) => {
                    signal = next;
                    forward_signal_or_warn(child, next);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(CHILD_WAIT_INTERVAL),
            },
        }
    }
}

#[cfg(unix)]
fn next_signal(signals: &Receiver<i32>) -> Option<i32> {
    signals.try_recv().ok()
}

#[cfg(unix)]
fn forward_signal_or_warn(child: &std::process::Child, signal: i32) {
    if let Err(e) = forward_signal(child, signal)
        && e.raw_os_error() != Some(libc::ESRCH)
    {
        logline!("clauth: failed to forward signal to claude: {e}");
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn forward_signal(child: &std::process::Child, signal: i32) -> std::io::Result<()> {
    // SAFETY: `child.id()` is the OS pid for this live child; `signal` comes from signal-hook.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `clauth start <codex-profile>` — spawn an interactive `codex` against this
/// session's own clauth-built home. The claude start's extras have no codex
/// counterpart and are absent on purpose: no usage priming (codex usage is
/// passive), no run-window transcript stamping or rescue (codex owns its own
/// `sessions/`, synced back by the runtime's teardown), no fallback watchdog
/// (a codex chain lands at the next start), no home-project settings guard
/// (project-tier settings are a Claude Code concept).
///
/// `codex_args` pass through verbatim, AFTER clauth's own `-c` store override
/// — later `-c` occurrences win in codex's config layering, but overriding
/// the store mode simply re-breaks the linked `auth.json` for that one run,
/// the same class of self-inflicted foot-gun as `claude --settings` against a
/// clauth runtime.
/// The spawn command for a codex session on `home`, split from [`run_codex`]
/// so the wire facts are pinned without spawning anything: the `CODEX_HOME`
/// pin, the scrub, and the forced file store BEFORE the passthrough args.
///
/// The store override (decision 6): the session's config.toml is a COPY of
/// the operator's, and a keyring/auto setting in it would make codex ignore
/// the linked auth.json — and delete it on the first refresh. Forced on every
/// clauth spawn, never demanded of the operator's own config (the capture
/// path owns that refusal). The value carries its TOML quotes as literal arg
/// bytes, so codex's `-c` override parser reads a well-formed TOML string
/// rather than leaning on its bare-word fallback. Caller args come AFTER, so
/// a later `-c` of the same key wins in codex's layering — overriding the
/// store re-breaks the linked auth.json for that one run, the same class of
/// self-inflicted foot-gun as `claude --settings` against a clauth runtime.
fn codex_spawn_command(
    home: &Path,
    codex_args: &[String],
    active_env_keys: &[String],
) -> std::process::Command {
    let engine = crate::harness::Harness::Codex.engine();
    let mut command = engine.command();
    engine.scrub_env(&mut command, active_env_keys);
    command.env(engine.home_env_key(), home);
    command.arg("-c").arg("cli_auth_credentials_store=\"file\"");
    command.args(codex_args);
    command
}

pub(crate) fn run_codex(
    config: &AppConfig,
    name: &str,
    codex_args: &[String],
    isolation: Isolation,
) -> Result<()> {
    // The ACTIVE CLAUDE profile's custom env, scrubbed like any spawn: those
    // keys reached this process from the live settings.json and describe a
    // claude account, not this codex session.
    let active_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .map(crate::profile::ProfileName::from)
        .and_then(|n| config.find(&n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing codex home");
        crate::runtime::CodexRuntime::acquire(name, isolation)?
    };

    let mut command = codex_spawn_command(runtime.home(), codex_args, &active_env_keys);

    // The same signal discipline as the claude start: without it a SIGTERM to
    // clauth skips the teardown — the sync-back is lost, the flock releases
    // with the codex child still RUNNING, and a rotation then reads the
    // account as idle while a live session holds its chain, which is the
    // precise burn the marker exists to prevent.
    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    let mut child = command.spawn().with_context(|| {
        "failed to launch codex — is the `codex` CLI installed and on PATH?".to_string()
    })?;

    #[cfg(unix)]
    let outcome = wait_for_child(&mut child, signal_watcher.receiver())?;
    #[cfg(not(unix))]
    let outcome = ChildOutcome {
        status: child
            .wait()
            .context("failed to wait for the session child")?,
        signal: None,
    };

    // Teardown before the exit so the sync-back and marker release run.
    drop(runtime);

    let code = status_code(outcome.status, outcome.signal);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/start.rs"]
mod tests;
