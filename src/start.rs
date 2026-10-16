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
use crate::out::errln;
use crate::profile::{AppConfig, Profile, ProfileName};
use crate::runtime::{Isolation, ProfileRuntime};
use crate::spinner::Spinner;

#[cfg(unix)]
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(50);

struct ChildOutcome {
    status: ExitStatus,
    signal: Option<i32>,
}

/// Lift an exiting isolated session's state into the global store, gated on
/// being the only live marker in `sessions`: the count — not the keying — is
/// what proves nothing is reading the tree being emptied, since the sidecar leg
/// would otherwise pull `shell-snapshots/` out from under a live Claude Code
/// mid-session. Self holds its own marker, hence `> 1`. The move itself lives
/// in [`crate::runtime::rescue_isolated_runtime`], shared with the stale-runtime
/// GC so an unrescued tree is lifted at its deletion site too.
///
/// Under real symlinks each session owns its tree and marker dir, and under
/// fake symlinks an isolated session owns them too, so the count is this
/// session alone and the `> 1` arm never fires in normal operation. Both arms
/// still earn their place. `None` means the marker dir could not be read;
/// deleting the guard would delete that refusal with no replacement.
/// `Some(n > 1)` is defence in depth against a same-sid collision and against a
/// legacy shared dir.
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
    crate::runtime::rescue_isolated_runtime(iso_root, claude_home)
}

/// The refusal a `--with-fallback` start gets on a host that structurally cannot
/// execute a per-session credential swap. Split from the gate so the render is
/// exercisable from any run: [`LinkMode::Fake`] is unreachable on a real-symlink
/// host.
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
/// order, and the transport probe runs last. That ordering is load-bearing: a
/// start refused for a cause the user can act on never materializes a profile
/// dir for an account that never launched.
fn refuse_unless_chain_eligible(
    config: &AppConfig,
    profile: &crate::profile::Profile,
    isolation: Isolation,
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

/// The refusals every start runs before any side effect, shared by [`run`] and
/// `cmd_start`'s explain/launch paths so `--explain` answers what a real launch
/// would do. The disabled gate is the authoritative one: every caller inherits
/// it here, before runtime acquire or spawn, so no caller can forget to check.
pub(crate) fn admit<'a>(
    config: &'a AppConfig,
    name: &ProfileName,
    isolation: Isolation,
    follows_chain: bool,
) -> Result<&'a Profile> {
    crate::refuse_if_disabled(config, name)?;
    let profile = config.find(name).context("profile not found")?;
    if follows_chain {
        refuse_unless_chain_eligible(config, profile, isolation)?;
    }
    Ok(profile)
}

/// The model strings a launch may run, from every source a launcher can see
/// before the session exists: the live `settings.json`, the process environment,
/// and an explicit `--model` passthrough. A UNION, not a resolution — a `Task`
/// subagent shares the parent's process-wide credential memo, so it spends the
/// same account on whatever model it runs, and selecting for the headline model
/// alone strands it (see [`crate::fallback::start_walk`]).
pub(crate) fn launch_models(claude_args: &[String]) -> Vec<String> {
    launch_models_from(
        crate::claude::claude_settings_models().unwrap_or_default(),
        [
            std::env::var("ANTHROPIC_MODEL").ok(),
            std::env::var("CLAUDE_CODE_SUBAGENT_MODEL").ok(),
        ],
        claude_args,
    )
}

/// The union of the three model sources a launcher can see, held here so the
/// union is testable without a home or a process environment: the live
/// `settings.json` strings, the two process-env strings (in order), and the
/// passthrough args. An empty env value is dropped, never a family.
pub(crate) fn launch_models_from(
    settings: Vec<String>,
    env: [Option<String>; 2],
    args: &[String],
) -> Vec<String> {
    let mut out = settings;
    out.extend(env.into_iter().flatten().filter(|v| !v.trim().is_empty()));
    out.extend(models_from_args(args));
    out
}

/// The `--model` and `--fallback-model` values in a passthrough arg list, in
/// both spellings. A `--fallback-model` value is a comma-separated list of
/// models, split and trimmed here. Split out of [`launch_models`] so the
/// parsing is testable without a home (its siblings read `settings.json` and
/// the environment, which resolve the operator's real home outside a sandbox).
pub(crate) fn models_from_args(claude_args: &[String]) -> Vec<String> {
    fn comma_list(out: &mut Vec<String>, v: &str) {
        out.extend(
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        );
    }

    let mut out = Vec::new();
    let mut args = claude_args.iter();
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--model=") {
            out.push(v.to_owned());
        } else if let Some(v) = a.strip_prefix("--fallback-model=") {
            comma_list(&mut out, v);
        } else if a == "--model"
            && let Some(v) = args.next()
        {
            out.push(v.clone());
        } else if a == "--fallback-model"
            && let Some(v) = args.next()
        {
            comma_list(&mut out, v);
        }
    }
    out
}

pub(crate) fn run(
    config: &AppConfig,
    name: &ProfileName,
    claude_args: &[String],
    isolation: Isolation,
    workspace: Option<&Path>,
    follows_chain: bool,
    announce: Option<&str>,
) -> Result<()> {
    let profile = admit(config, name, isolation, follows_chain)?;

    // Announced after the refusals, so a start that `admit` refuses never
    // announces first.
    if let Some(line) = announce {
        errln!("{line}");
    }

    // The plugin-migration pre-flight: heal a broken or divergent clauth
    // marketplace registration before the session launches, so the session loads
    // its hooks and MCP. A healthy registration costs this nothing — the gate is
    // two registry-file reads and spawns no `claude`. Best-effort: a failed heal
    // is logged, never fails the start. `claude` the binary (not just the plugin
    // CLI) needs to be on PATH anyway for the spawn below to succeed, and an
    // uninstalled plugin heals to a one-read no-op.
    crate::plugin_host::preflight();

    // Strip the outgoing profile's custom env from the inherited base so a
    // `clauth start <other>` session doesn't inherit it. The live
    // `settings.json` is owned by whoever is active; starting that same profile
    // passes its own keys, which the merge re-inserts (no-op). With no active
    // marker to read (`switch_off` clears it without touching the file) the
    // helper answers every configured profile's keys, so a departed account's
    // entries are stripped too rather than landing in front of the started
    // account's endpoint.
    let stale_env_keys = crate::actions::outgoing_env_keys(config);

    let runtime = {
        let _spinner = Spinner::start("clauth: preparing runtime");
        ProfileRuntime::acquire(profile, isolation, &stale_env_keys, follows_chain)?
    };

    #[cfg(unix)]
    let signal_watcher = SignalWatcher::new()?;

    // Through the runtime-spawn seam: this is a claude session, and the codex
    // engine plugs into the same three calls when its runtime lands.
    let engine: &dyn crate::harness::HarnessEngine = &crate::harness::ClaudeEngine;
    let mut command = engine.command();
    // Scrub clauth-managed + outgoing custom env so a session started under
    // profile B doesn't inherit profile A's endpoint/auth/model overrides from
    // the parent process env. The target's runtime settings.json re-supplies
    // whichever it defines. Mirrors the delegate path (run_delegate).
    engine.scrub_env(&mut command, &stale_env_keys);
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
    // Through the shared `projects` link, the default auto-memory dir loses
    // Claude Code's write exemption; name the resolved one instead.
    if let Some(cwd) = spawn_cwd.as_deref() {
        crate::memory_dir::pin_auto_memory_dir(
            &mut command,
            cwd,
            runtime.config_dir(),
            claude_args,
        );
    }
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

/// A path rendered as a TOML string for a `-c key=<value>` override. Literal
/// (single-quoted) is preferred: it processes no escapes, so a Windows path's
/// backslashes survive verbatim. A path carrying a single quote cannot be a
/// literal string at all, so it falls back to a basic string with the two
/// characters TOML requires escaped there.
fn toml_path_value(path: &Path) -> String {
    let raw = path.display().to_string();
    if raw.contains('\'') {
        format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{raw}'")
    }
}

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
///
/// The state-DB override, the same reasoning one layer down: the store the
/// sqlite DBs live in is a config KEY (`sqlite_home`) as well as an env var,
/// and the session's config.toml is that same copy of the operator's. Left
/// alone, every profile's goals/logs/memories/state DBs land in whichever one
/// directory the operator named — the home's durable links are never opened
/// through, and two accounts share one conversation history. Scrubbing the env
/// cannot reach it, since the key outranks the variable, so it is pinned to the
/// home clauth just set: exactly what codex resolves when neither is spelled.
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
    command
        .arg("-c")
        .arg(format!("sqlite_home={}", toml_path_value(home)));
    command.args(codex_args);
    command
}

/// What codex's managed config does to a clauth-built home. Read before a
/// codex spawn: the managed layer sits ABOVE the session's `-c` flags in
/// codex's config stack (`config_layer_source.rs`: session flags 30, the
/// managed file 40), so a key set there defeats the forced store and state-DB
/// home that [`codex_spawn_command`] pins, and nothing clauth passes can
/// outrank it.
#[derive(Debug, PartialEq, Eq)]
enum ManagedConfigVerdict {
    /// Nothing there reaches the keys clauth forces or strips.
    Clear,
    /// The spawn proceeds; the line goes to stderr.
    Warn(String),
    /// The spawn is refused with the line.
    Refuse(String),
}

/// Where codex reads its managed config: the system path on unix. On windows
/// codex defaults it to `<CODEX_HOME>/managed_config.toml`, and the session's
/// `CODEX_HOME` is the home clauth just built, which holds none.
fn managed_config_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = MANAGED_CONFIG_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return Some(path);
    }
    #[cfg(unix)]
    {
        Some(PathBuf::from("/etc/codex/managed_config.toml"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Test-only [`managed_config_path`] override: the system path is root-owned,
/// so the spawn-site read in [`run_codex`] is otherwise unreachable from a
/// test. Serialized by `profile::HOME_TEST_LOCK`. Never compiled into the
/// binary.
#[cfg(test)]
static MANAGED_CONFIG_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn set_managed_config_override(path: &Path) {
    if let Ok(mut guard) = MANAGED_CONFIG_OVERRIDE.lock() {
        *guard = Some(path.to_path_buf());
    }
}

#[cfg(test)]
fn clear_managed_config_override() {
    if let Ok(mut guard) = MANAGED_CONFIG_OVERRIDE.lock() {
        *guard = None;
    }
}

/// Test-only RAII: point the spawn-site managed-config read at `path` for the
/// guard's lifetime, clearing the process-global override on drop even if the
/// test panics. It BORROWS the [`HomeSandbox`](crate::testutil::HomeSandbox)
/// for the reason `testutil::EndpointSandbox` does: the override is serialized
/// by `HOME_TEST_LOCK`, which the home sandbox holds, so dropping the home
/// first is E0505 at compile time instead of a race nothing checks.
#[cfg(test)]
struct ManagedConfigSandbox<'a>(std::marker::PhantomData<&'a crate::testutil::HomeSandbox>);

#[cfg(test)]
impl<'a> ManagedConfigSandbox<'a> {
    fn new(_home: &'a crate::testutil::HomeSandbox, path: &Path) -> Self {
        set_managed_config_override(path);
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for ManagedConfigSandbox<'_> {
    fn drop(&mut self) {
        clear_managed_config_override();
    }
}

/// The verdict over the managed file at `path`. Absent, unreadable or
/// unparseable is [`ManagedConfigVerdict::Clear`]: there is nothing to
/// outrank the spawn with, and a file codex cannot parse is codex's own
/// refusal. The keys are exactly the ones [`codex_spawn_command`] forces and
/// `runtime::copy_codex_config` strips: the two that kill the chain refuse
/// (a non-file store unbinds the linked `auth.json`: codex reads the keyring
/// or memory past it, and `keyring`/`auto` delete it on their first save; a
/// lockfile `load_path` replays that file as the WHOLE config, erasing the
/// `-c` layer, where the table's other keys only export), and a moved
/// state-DB home warns, since the session still runs on its own chain.
fn managed_config_verdict(path: &Path) -> ManagedConfigVerdict {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return ManagedConfigVerdict::Clear;
    };
    let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
        return ManagedConfigVerdict::Clear;
    };
    let Some(table) = parsed.as_table() else {
        return ManagedConfigVerdict::Clear;
    };
    let file = path.display();
    if let Some(store) = table.get("cli_auth_credentials_store")
        && store.as_str() != Some("file")
    {
        return ManagedConfigVerdict::Refuse(format!(
            "{file} sets cli_auth_credentials_store = {store}, and a managed config outranks \
             the file store clauth forces at spawn, so codex would ignore this session's \
             linked auth.json. ask whoever manages this machine to remove the key or set it \
             to \"file\"; clauth cannot override a managed config"
        ));
    }
    if let Some(load_path) = table
        .get("debug")
        .and_then(toml::Value::as_table)
        .and_then(|debug| debug.get("config_lockfile"))
        .and_then(toml::Value::as_table)
        .and_then(|lockfile| lockfile.get("load_path"))
    {
        return ManagedConfigVerdict::Refuse(format!(
            "{file} sets debug.config_lockfile.load_path = {load_path}, and a managed config \
             outranks the flags clauth passes at spawn, so codex would replay that lockfile \
             as its whole config and drop the file store this session's linked auth.json \
             depends on. ask whoever manages this machine to remove the key; clauth cannot \
             override a managed config"
        ));
    }
    if let Some(sqlite_home) = table.get("sqlite_home") {
        return ManagedConfigVerdict::Warn(format!(
            "{file} sets sqlite_home = {sqlite_home}, which outranks the per-session home \
             clauth pins at spawn, so every profile's state dbs land in that one directory"
        ));
    }
    ManagedConfigVerdict::Clear
}

/// `clauth start <codex-profile>` — spawn an interactive `codex` against this
/// session's own clauth-built home. The claude start's extras have no codex
/// counterpart and are absent on purpose: no usage priming (codex usage is
/// passive), no run-window transcript stamping or rescue (codex owns its own
/// `sessions/`, which the shared flavor links into the profile store), no
/// fallback watchdog (a codex chain lands at the next start), no home-project
/// settings guard (project-tier settings are a Claude Code concept).
///
/// `codex_args` pass through verbatim, AFTER clauth's own `-c` store override
/// — later `-c` occurrences win in codex's config layering, but overriding
/// the store mode simply re-breaks the linked `auth.json` for that one run,
/// the same class of self-inflicted foot-gun as `claude --settings` against a
/// clauth runtime.
pub(crate) fn run_codex(
    config: &AppConfig,
    name: &str,
    codex_args: &[String],
    isolation: Isolation,
) -> Result<()> {
    // Before the runtime exists: a refused start never materializes a home.
    if let Some(path) = managed_config_path() {
        match managed_config_verdict(&path) {
            ManagedConfigVerdict::Clear => {}
            ManagedConfigVerdict::Warn(line) => errln!("clauth: {line}"),
            ManagedConfigVerdict::Refuse(line) => anyhow::bail!(line),
        }
    }

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
    // clauth skips the teardown — its carry-backs are lost, the flock releases
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

    // Teardown before the exit so the carry-backs and marker release run.
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
