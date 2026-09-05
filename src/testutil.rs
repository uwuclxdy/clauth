//! Shared test-only helpers used across the inline test modules
//! (`tests/inline/*.rs`). Defined once here rather than copied per module so the
//! home-sandbox, mtime, and key-event scaffolding stays in a single place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// RAII home sandbox: acquires `HOME_TEST_LOCK` and redirects `home_dir()` into
/// a tempdir for its lifetime, clearing the override on drop (even on panic).
/// Required for any test that writes into the per-profile tree or creates
/// session dirs, pid files, or rotation locks — otherwise those paths land in
/// the real `~/.clauth`.
pub(crate) struct HomeSandbox {
    // Drop order: tempdir first, then the shared lock.
    _tmp: tempfile::TempDir,
    _guard: crate::lockorder::RankedGuard<'static, ()>,
    home: PathBuf,
    prev_config_dir: Option<std::ffi::OsString>,
}

impl HomeSandbox {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, acquired above"
    )]
    pub(crate) fn new() -> Self {
        // Untracked HOME_TEST_LOCK acquired first; no RankedMutex/flock is held.
        let guard = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("create home sandbox");
        let home = tmp.path().to_path_buf();
        crate::profile::set_home_override(home.clone());
        // `home_override` does not reach `CLAUDE_CONFIG_DIR`, and the operator's
        // own value names a runtime OUTSIDE this tempdir: `which::session_auth`
        // and `which::resolve_active` honor it, so a test run from inside a
        // `clauth start` session resolves the real `~/.claude/.credentials.json`
        // and reads a file it never staged. Clearing it here makes the sandbox
        // the only answer, the rule `profile::home_dir` already states for the
        // home. A test that WANTS one pins it with [`ConfigDirSandbox`], which
        // borrows this guard and so always sets it after this clear.
        let prev_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, restored on drop.
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        Self {
            _tmp: tmp,
            _guard: guard,
            home,
            prev_config_dir,
        }
    }

    /// Path to the sandboxed home directory.
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for HomeSandbox {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, still held here"
    )]
    fn drop(&mut self) {
        // Join BEFORE clearing the override, not after and not per-test. A
        // detached worker still running when `HOME_OVERRIDE` clears resolves
        // the operator's REAL `$HOME` and takes real locks under `~/.clauth`
        // (`RotationGuard::acquire` alone does `mkdir_700` + a blocking
        // flock). Doing it here rather than asking each test to call the join
        // fns covers the tests that never thought about it — which is every
        // test that will ever be added. Two registries: the TUI's own
        // `spawn_worker` handles (joinable OS threads) and
        // [`join_background_tasks`] for detach mechanisms that hand back no
        // joinable handle at all (e.g. `tokio::task::spawn_blocking`).
        crate::tui::join_test_workers();
        join_background_tasks();
        crate::profile::clear_home_override();
        // Restore the operator's own value, after the joins above for the same
        // reason the home override is cleared there: a still-running worker that
        // sees it back resolves outside the sandbox.
        // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, still held.
        match self.prev_config_dir.take() {
            Some(prev) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", prev) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
    }
}

/// Completion signals for detached background tasks that have no joinable
/// handle of their own — e.g. the MCP background delegate, which detaches via
/// `tokio::task::spawn_blocking` and drops the returned task handle
/// immediately (see `mcp::launch_background_delegate`). Hoisted here rather
/// than living beside `tui::TEST_WORKERS` because a second subsystem now
/// needs the same join: this is the shared test-helper home, not
/// TUI-specific. Never compiled into the binary.
#[cfg(test)]
static BACKGROUND_TASK_DONE: std::sync::Mutex<Vec<std::sync::mpsc::Receiver<()>>> =
    std::sync::Mutex::new(Vec::new());

/// How long [`join_background_tasks`] waits on one detached task before it
/// gives up and says so. A hang detector, not a race bound: every task that
/// registers here finishes in milliseconds, so nothing legitimate comes close.
/// The point of bounding it at all is that an unbounded `recv` turns a stuck
/// task into a CI job that times out with no output naming what it was waiting
/// on.
#[cfg(test)]
const BACKGROUND_TASK_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Register a detached task's completion receiver so [`HomeSandbox::drop`]
/// can block on it before it clears the home override. The returned sender is
/// the task's contract: send on it as the LAST action, after every
/// `$HOME`-touching step (config load, disk write) is done. A guard bound
/// inside the task drops in reverse declaration order and therefore lands
/// AFTER the send, so any guard whose `Drop` reaches `$HOME` has to be dropped
/// explicitly before it.
///
/// Panics when no home sandbox is alive. The registry is process-global, so a
/// receiver pushed with nothing to drain it sits there until some later,
/// unrelated test's teardown blocks on a task that test never launched — a
/// failure that names the wrong test and reads as a hang in it. Failing at the
/// registration names the caller instead.
#[cfg(test)]
pub(crate) fn register_background_task() -> std::sync::mpsc::Sender<()> {
    assert!(
        crate::profile::home_override_active(),
        "a background task was registered with no home sandbox alive — hold a \
         `HomeSandbox` across the launch, or its completion signal outlives this \
         test and blocks an unrelated one's teardown"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut done) = BACKGROUND_TASK_DONE.lock() {
        done.push(rx);
    }
    tx
}

/// How many registered tasks have not been joined yet. Lets a test pin that a
/// driver joined its own detached work rather than leaving it to teardown.
#[cfg(test)]
pub(crate) fn pending_background_tasks() -> usize {
    BACKGROUND_TASK_DONE.lock().map(|d| d.len()).unwrap_or(0)
}

/// Block until every task registered via [`register_background_task`] has
/// signaled completion, or [`BACKGROUND_TASK_JOIN_TIMEOUT`] elapses on one.
///
/// A disconnected channel is a finished wait, not a failure: a task that
/// panics drops its sender while unwinding, and the panic itself is what the
/// run should report.
///
/// Callable while a runtime is still alive, which is the point — a
/// `tokio::task::spawn_blocking` task is scheduled NON-MANDATORY, so dropping
/// its runtime while it is still queued discards it un-run (measured: two
/// detached fan-out delegates spawned, one never entered its closure, its job
/// left `running` past 120s). Draining here, before the drop, is what makes the
/// task's completion something a test can rely on.
#[cfg(test)]
pub(crate) fn join_background_tasks() {
    join_background_tasks_with(BACKGROUND_TASK_JOIN_TIMEOUT);
}

/// [`join_background_tasks`] against a caller-supplied bound, so the timeout
/// branch can be exercised without a test that waits out the real one.
#[cfg(test)]
pub(crate) fn join_background_tasks_with(timeout: std::time::Duration) {
    let pending: Vec<_> = BACKGROUND_TASK_DONE
        .lock()
        .map(|mut d| std::mem::take(&mut *d))
        .unwrap_or_default();
    for rx in pending {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
            let msg = format!(
                "a detached background task did not signal completion within {:?} \
                 (`testutil::join_background_tasks`); it is stuck before its final \
                 send, so the home override cannot be cleared safely",
                timeout
            );
            // This runs inside `HomeSandbox::drop`, where panicking during an
            // unwind aborts the process and buries the original failure.
            if std::thread::panicking() {
                crate::out::errln!("clauth: {msg}");
            } else {
                panic!("{msg}");
            }
        }
    }
}

/// Every delegate job in the sandbox's store, as `id=state` pairs, sorted so a
/// failure message reads the same twice.
#[cfg(test)]
fn job_states() -> Vec<(String, crate::mcp::jobs::JobState)> {
    let mut out: Vec<_> = crate::mcp::jobs::jobs_dir()
        .ok()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let id = e.path().file_stem()?.to_str()?.to_string();
                    let record = crate::mcp::jobs::read(&id)?;
                    Some((id, record.state))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Assert that exactly `count` delegate jobs in the sandbox's store reached
/// `done` — so a job left at `running` reds, which is the shape the wall clock
/// this replaces reported as a timeout.
///
/// A plain assertion, not a poll. Every driver joins its detached tasks before
/// it returns, and a task writes its job file BEFORE its completion send, so
/// the store is final by the time a test reaches here. It replaces two copies
/// of a helper that polled a 10s wall clock instead — which reported a task
/// that never ran as a timeout, always within 60ms of the ceiling, in one
/// whole-suite release run out of three, in a module the diff under test never
/// touched.
#[cfg(test)]
pub(crate) fn assert_jobs_done(count: usize) {
    let states = job_states();
    let done = states
        .iter()
        .filter(|(_, state)| *state == crate::mcp::jobs::JobState::Done)
        .count();
    assert_eq!(
        done, count,
        "every job the run created is finalized once its driver returns: {states:?}"
    );
}

// ── printable-escape-hatch probes ────────────────────────────────────────────
//
// The error types that hold upstream-derived facts (`oauth::TokenFailure`,
// `RefreshError`, `KickError`, `oauth_login::AuthorizeRejection`) are contained
// by NOT being printable: no `Display` means `{e}` does not compile, and no
// `Into<anyhow::Error>` means `?` cannot launder one into something that does.
// Both properties are invisible to a normal assertion, so probe them: the
// inherent method wins method lookup whenever its bound holds, and lookup falls
// through to the blanket trait method when it does not.

pub(crate) struct Probe<T>(std::marker::PhantomData<T>);

impl<T: std::fmt::Display> Probe<T> {
    pub(crate) fn is_display() -> bool {
        true
    }
}

impl<T: Into<anyhow::Error>> Probe<T> {
    pub(crate) fn into_anyhow() -> bool {
        true
    }
}

pub(crate) trait NotDisplay {
    fn is_display() -> bool {
        false
    }
}

impl<T> NotDisplay for Probe<T> {}

pub(crate) trait NotIntoAnyhow {
    fn into_anyhow() -> bool {
        false
    }
}

impl<T> NotIntoAnyhow for Probe<T> {}

impl<T: Send> Probe<T> {
    pub(crate) fn is_send() -> bool {
        true
    }
}

pub(crate) trait NotSend {
    fn is_send() -> bool {
        false
    }
}

impl<T> NotSend for Probe<T> {}

// ── offline rotation-leg harness ─────────────────────────────────────────────
//
// Every rotation decision sits BEHIND an HTTP call, so a refusal deleted from
// `fetch_with_rotation`, `auto_start_kick` or `rotate_one_inner` is invisible to
// any test that cannot answer that call. These live here rather than beside one
// test module because both the scheduler and the oauth suites drive those legs.

/// A loopback stand-in for the Anthropic hosts, answering by request PATH so a
/// leg's request ORDER isn't baked into the fixture. Serves up to `max` requests
/// and returns the path of each one it saw, in order.
///
/// `max` must be set ABOVE what a correct run makes, never equal to it. A
/// must-NOT-call assertion (`!seen.contains(token_endpoint)`) is only meaningful
/// if the listener would have accepted and recorded that call — a `max` sized to
/// the happy path makes the forbidden request invisible and the assertion passes
/// no matter what the code does. That exact fixture bug let a deleted refusal
/// stay green here once already.
///
/// The listener is NON-BLOCKING with two deadlines. A leg that refuses early
/// makes fewer requests than `max`, and a blocking `accept` would hang the suite
/// instead of failing it — the shape a restored refusal has, so the harness
/// would swallow the very mutation it exists to catch. `IDLE_GRACE` is what
/// bounds the "nothing more is coming" case, and must stay above the 5s per-host
/// request spacing or a paced follow-up reads as absent.
pub(crate) fn serve_endpoints(
    max: usize,
    reply: impl Fn(&str, usize) -> (u16, String) + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// Long enough for a leg that sleeps on pacing before its FIRST request.
    const FIRST_WAIT: Duration = Duration::from_secs(45);
    /// Above `REQUEST_SPACING_MS` (5s) plus the kick's 2s step delay.
    const IDLE_GRACE: Duration = Duration::from_secs(12);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let handle = std::thread::spawn(move || {
        let mut seen: Vec<String> = Vec::new();
        for i in 0..max {
            let deadline = Instant::now()
                + if seen.is_empty() {
                    FIRST_WAIT
                } else {
                    IDLE_GRACE
                };
            let mut sock = loop {
                if Instant::now() > deadline {
                    return seen;
                }
                match listener.accept() {
                    Ok((sock, _)) => break sock,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return seen,
                }
            };
            sock.set_nonblocking(false).ok();
            sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            // Drain headers AND any Content-Length body before replying: a
            // close with unread bytes RSTs the client on Windows.
            let mut req = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        req.extend_from_slice(&tmp[..n]);
                        if let Some(h) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            let len = String::from_utf8_lossy(&req[..h])
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if req.len() >= h + 4 + len {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&req).into_owned();
            let path = text
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let (status, body) = reply(&path, i);
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = sock.write_all(body.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Write);
            seen.push(path);
        }
        seen
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

pub(crate) fn rotation_fixture_config(
    name: &crate::profile::ProfileName,
) -> crate::profile::ConfigHandle {
    let mut profile = blank_profile(name);
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-old".into(),
            refresh_token: Some("rt-old".into()),
            // Far outside the lead window, so the leg is REACTIVE: the 401 is
            // what drives it, not the proactive predicate.
            expires_at: Some(crate::usage::now_ms() as i64 + 86_400_000),
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.clone());
    config.state.active_profile = Some(name.clone());
    crate::profile::save_app_state(&config.state).expect("save app state");
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(config))
}

/// Make the next `save_profile` for `name` fail on its credentials write, so a
/// rotation's persist leg can be driven without root or a mode flip: the write
/// goes through `atomic_write_600`, whose `rename(tmp, credentials.json)` is
/// `EISDIR` once a DIRECTORY sits at that path.
///
/// Aimed at `credentials.json` rather than the profile dir on purpose: it denies
/// exactly the write under test and nothing else, so the leg reaches its persist
/// and fails there. Breaking the whole profile dir would also deny `config.toml`
/// and every session path, making which write failed unattributable. (It used to
/// fail `RotationGuard::acquire` outright, when the rotation lock still lived in
/// that directory; the lock has since moved out of it.)
///
/// Gated with its callers: both are non-macOS tests, and an ungated helper with
/// no macOS caller is a dead-code error that reds that leg on clippy
/// `-D warnings` before a test runs.
#[cfg(not(target_os = "macos"))]
pub(crate) fn block_credentials_write(name: &crate::profile::ProfileName) {
    let path = crate::profile::profile_subpath(name, "credentials.json").expect("credentials path");
    if path.is_file() {
        std::fs::remove_file(&path).expect("drop the fixture's credentials file");
    }
    std::fs::create_dir(&path).expect("block the credentials path with a directory");
}

/// RAII pin redirecting every Anthropic endpoint the rotation legs touch —
/// `/usage`, the token endpoint, and the `/v1/messages` kick — at one loopback
/// listener, and clearing them on drop even if the test panics. Also resets the
/// per-host request spacing, or the second request in a leg sleeps out
/// `REQUEST_SPACING_MS`, and the adopt's token → uuid memo, which is
/// process-lifetime: a fake token two tests share would otherwise let the first
/// answer the second's probe and delete the very request it asserts on.
///
/// Rotation decisions all sit BEHIND an HTTP call, so without this the refusals
/// in `fetch_with_rotation` / `auto_start_kick` / `rotate_one_inner` are covered
/// by nothing.
///
/// It BORROWS the [`HomeSandbox`] rather than documenting "outlive me": the
/// overrides are process-globals serialized by `HOME_TEST_LOCK`, which the home
/// sandbox holds, so dropping the home first would release that lock while the
/// overrides are still set and let the next test run against them. As a borrow
/// that inversion is E0505 at compile time instead of a race nothing checks.
pub(crate) struct EndpointSandbox<'a>(std::marker::PhantomData<&'a HomeSandbox>);

impl<'a> EndpointSandbox<'a> {
    /// Point every endpoint at `base` (an `http://127.0.0.1:PORT` listener).
    pub(crate) fn new(_home: &'a HomeSandbox, base: &str) -> Self {
        crate::oauth::set_endpoint_overrides(
            &format!("{base}/v1/oauth/token"),
            &format!("{base}/v1/messages?beta=true"),
        );
        crate::usage::set_usage_endpoint_override(
            &format!("{base}/api/oauth/usage"),
            &format!("{base}/api/oauth/profile"),
        );
        crate::usage::reset_request_slots();
        crate::usage::reset_identity_memo();
        crate::oauth::reset_stored_probe_suppression();
        Self(std::marker::PhantomData)
    }
}

impl Drop for EndpointSandbox<'_> {
    fn drop(&mut self) {
        crate::oauth::clear_endpoint_overrides();
        crate::usage::clear_usage_endpoint_override();
        crate::usage::reset_request_slots();
        crate::usage::reset_identity_memo();
        crate::oauth::reset_stored_probe_suppression();
    }
}

/// RAII `CLAUDE_CONFIG_DIR` pin: forces the var for its lifetime and restores the
/// previous value on drop (even on panic). Required by any test exercising a path
/// that reads the session's config dir — `which::session_auth`,
/// `which::resolve_active`, and everything attributing loaded credentials.
///
/// It BORROWS the [`HomeSandbox`] for the same reason [`EndpointSandbox`] does:
/// the env is a process-global serialized by `HOME_TEST_LOCK`, which the home
/// sandbox holds, so dropping the home first would release that lock with this
/// pin still standing and let the next test run against it. As a borrow that
/// inversion is E0505 at compile time instead of a race nothing checks.
pub(crate) struct ConfigDirSandbox<'a> {
    _pin: EnvPin<'a>,
}

impl<'a> ConfigDirSandbox<'a> {
    pub(crate) fn new(home: &'a HomeSandbox, dir: &Path) -> Self {
        Self {
            _pin: EnvPin::new(home, &[("CLAUDE_CONFIG_DIR", Some(dir.as_os_str()))]),
        }
    }
}

/// One or more process env pins, restored to their previous values on drop
/// (even on panic), in reverse order. Same contract as every pin here: the env
/// is process-global, serialized by `HOME_TEST_LOCK`, so the pin BORROWS the
/// [`HomeSandbox`] that holds it; dropping the home first is E0505 at compile
/// time instead of a race nothing checks.
pub(crate) struct EnvPin<'a> {
    prevs: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _home: std::marker::PhantomData<&'a HomeSandbox>,
}

impl<'a> EnvPin<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    pub(crate) fn new(
        _home: &'a HomeSandbox,
        pins: &[(&'static str, Option<&std::ffi::OsStr>)],
    ) -> Self {
        let mut prevs = Vec::with_capacity(pins.len());
        for &(key, value) in pins {
            let prev = std::env::var_os(key);
            // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, restored on drop.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            prevs.push((key, prev));
        }
        Self {
            prevs,
            _home: std::marker::PhantomData,
        }
    }
}

impl Drop for EnvPin<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn drop(&mut self) {
        // SAFETY: same as `new` — restore the prior values under the same lock.
        for (key, prev) in self.prevs.iter().rev() {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// The body of the fake `claude` the agentgear lifecycle pins stage.
/// `@VERSION@` is substituted with the crate version at write time so the
/// entry the shim reports is never stale against the embedded tree.
#[cfg(unix)]
const FAKE_CLAUDE_SHIM: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$CLAUDE_SHIM_LOG"
case "$1" in
  --version)
    echo "2.1.220 (Claude Code)"
    ;;
  plugin)
    case "$2" in
      list)
        # [] until an install happened, then the clauth entry with an existing
        # installPath (agentgear's verify_present checks the path on disk).
        if [ "$3" = "--json" ]; then
          if [ -f "$CLAUDE_SHIM_STATE" ]; then
            printf '[{"id":"clauth@clauth","version":"@VERSION@","enabled":true,"installPath":"%s"}]\n' "$CLAUDE_SHIM_TREE"
          else
            echo '[]'
          fi
        fi
        ;;
      marketplace)
        case "$3" in
          list)
            # The registered marketplace as `marketplace list --json` reads it:
            # recorded at add time, so agentgear's probe can compare the path.
            if [ -f "$CLAUDE_SHIM_MKT_STATE" ]; then
              printf '[{"name":"clauth","path":"%s"}]\n' "$(cat "$CLAUDE_SHIM_MKT_STATE")"
            else
              echo '[]'
            fi
            ;;
          add)
            # Re-add over the same name re-points, like the real CLI.
            printf '%s\n' "$4" > "$CLAUDE_SHIM_MKT_STATE"
            ;;
        esac
        ;;
      install)
        : > "$CLAUDE_SHIM_STATE"
        # The registry clauth's own probe reads: write the user-scope entry so
        # the Plugin tab recompute after the install sees it.
        mkdir -p "$CLAUDE_CONFIG_DIR/plugins"
        printf '{"plugins":{"clauth@clauth":[{"scope":"user","version":"@VERSION@","installedAt":"2026-08-25T00:00:00.000Z","installPath":"%s"}]}}\n' "$CLAUDE_SHIM_TREE" > "$CLAUDE_CONFIG_DIR/plugins/installed_plugins.json"
        ;;
    esac
    ;;
esac
exit 0
"#;

/// A stateful fake `claude` on a PATH prefix, plus the hermetic env pins the
/// agentgear lifecycle needs (home, data dir, runtime dir, the shim's own
/// vars). It
/// mutates process-global env, so it BORROWS the [`HomeSandbox`] whose
/// `HOME_TEST_LOCK` serializes every other env pin in the suite (the
/// `ConfigDirSandbox` pattern). The prefix keeps the original PATH behind it,
/// so concurrent tests that spawn `sh`/`true` still resolve them; only the
/// FIRST `claude` hit changes, and nothing else in the test binary spawns
/// `claude` (the version/hdr/mcp probes are all `cfg!(test)`-skipped).
#[cfg(unix)]
pub(crate) struct FakeClaude<'a> {
    _home: std::marker::PhantomData<&'a HomeSandbox>,
    _tmp: tempfile::TempDir,
    log: std::path::PathBuf,
    prev_path: std::ffi::OsString,
    prev_home: Option<std::ffi::OsString>,
    prev_data: Option<std::ffi::OsString>,
    prev_runtime: Option<std::ffi::OsString>,
    prev_tree: Option<std::ffi::OsString>,
    prev_state: Option<std::ffi::OsString>,
    prev_mkt_state: Option<std::ffi::OsString>,
    prev_log: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl<'a> FakeClaude<'a> {
    /// The full harness: the shim on a PATH prefix ahead of the operator's
    /// own PATH.
    pub(crate) fn new(home: &'a HomeSandbox) -> Self {
        let prev_path = std::env::var_os("PATH").expect("PATH is set");
        Self::stage(home, &prev_path, true)
    }

    /// The same pins with a claude-free PATH (empty shim dir + a minimal
    /// tail), so agentgear's claude backend is undetected and the lifecycle
    /// converges to `NoOp` without spawning anything.
    pub(crate) fn new_without_claude(home: &'a HomeSandbox) -> Self {
        Self::stage(home, std::ffi::OsStr::new("/usr/bin:/bin"), false)
    }

    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn stage(home: &'a HomeSandbox, path_tail: &std::ffi::OsStr, with_shim: bool) -> Self {
        let tmp = tempfile::tempdir_in(home.home()).expect("shim dir");
        let data = tmp.path().join("data");
        let run = tmp.path().join("run");
        let tree = tmp.path().join("install-tree");
        for dir in [&data, &run, &tree] {
            std::fs::create_dir_all(dir).expect("create dir");
        }
        if with_shim {
            let shim = tmp.path().join("claude");
            std::fs::write(
                &shim,
                FAKE_CLAUDE_SHIM.replace("@VERSION@", env!("CARGO_PKG_VERSION")),
            )
            .expect("write shim");
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim).expect("shim meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).expect("chmod shim");
        }

        let pin = |key: &str, value: &std::path::Path| {
            let prev = std::env::var_os(key);
            // SAFETY: test-only, serialized by HOME_TEST_LOCK, restored on drop.
            unsafe { std::env::set_var(key, value) };
            prev
        };
        let prev_path = std::env::var_os("PATH").expect("PATH is set");
        let mut path = std::ffi::OsString::from(tmp.path());
        path.push(":");
        path.push(path_tail);
        // SAFETY: test-only, serialized by HOME_TEST_LOCK, restored on drop.
        unsafe { std::env::set_var("PATH", path) };
        let prev_data = pin("XDG_DATA_HOME", &data);
        // The home pin is what keeps `dirs`-based resolution inside the sandbox
        // on macOS: there `data_dir()` derives from `$HOME` alone and ignores
        // `XDG_DATA_HOME`, so the data pin above is a no-op for agentgear's
        // tree root.
        let prev_home = pin("HOME", home.home());
        let prev_runtime = pin("XDG_RUNTIME_DIR", &run);
        let prev_tree = pin("CLAUDE_SHIM_TREE", &tree);
        let prev_state = pin("CLAUDE_SHIM_STATE", &tmp.path().join("state"));
        let prev_mkt_state = pin("CLAUDE_SHIM_MKT_STATE", &tmp.path().join("mkt-state"));
        let log = tmp.path().join("log");
        let prev_log = pin("CLAUDE_SHIM_LOG", &log);
        Self {
            _home: std::marker::PhantomData,
            _tmp: tmp,
            log,
            prev_path,
            prev_home,
            prev_data,
            prev_runtime,
            prev_tree,
            prev_state,
            prev_mkt_state,
            prev_log,
        }
    }

    pub(crate) fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[cfg(unix)]
impl Drop for FakeClaude<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn drop(&mut self) {
        // SAFETY: restore the prior values under the same lock the sandbox holds.
        unsafe {
            std::env::set_var("PATH", &self.prev_path);
            for (key, value) in [
                ("XDG_DATA_HOME", &self.prev_data),
                ("HOME", &self.prev_home),
                ("XDG_RUNTIME_DIR", &self.prev_runtime),
                ("CLAUDE_SHIM_TREE", &self.prev_tree),
                ("CLAUDE_SHIM_STATE", &self.prev_state),
                ("CLAUDE_SHIM_MKT_STATE", &self.prev_mkt_state),
                ("CLAUDE_SHIM_LOG", &self.prev_log),
            ] {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Seed a plugin registration the heal gate must act on: a `clauth@clauth`
/// user-scope row whose `installPath` is gone. The registry lives under the
/// sandboxed claude dir, so this touches nothing outside it.
#[cfg(unix)]
pub(crate) fn seed_broken_plugin_registration() {
    let dir = crate::profile::claude_dir()
        .expect("claude dir")
        .join("plugins");
    std::fs::create_dir_all(&dir).expect("plugins dir");
    std::fs::write(dir.join("known_marketplaces.json"), "{}").expect("marketplaces");
    std::fs::write(
        dir.join("installed_plugins.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "plugins": {"clauth@clauth": [{"scope": "user", "installPath": "/gone/runtime/plugins/cache"}]}
        }))
        .expect("seed json"),
    )
    .expect("installed");
}

/// RAII tier pin: acquires `TIER_TEST_LOCK` and forces the process-global color
/// tier for its lifetime, putting the previous pin back on drop (even on panic).
/// Required for any test asserting on a tier-dependent style, since the tier is
/// process-global and otherwise auto-detects from the ambient `$COLORTERM`.
pub(crate) struct TierSandbox {
    // Drop order: this type's `drop` restores under the lock, which the field
    // then releases.
    _guard: crate::lockorder::RankedGuard<'static, ()>,
    prev: Option<crate::tui::theme::Tier>,
}

impl TierSandbox {
    pub(crate) fn new(tier: crate::tui::theme::Tier) -> Self {
        let guard = crate::tui::theme::TIER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = crate::tui::theme::tier_override();
        crate::tui::theme::set_tier(tier);
        Self {
            _guard: guard,
            prev,
        }
    }
}

impl Drop for TierSandbox {
    fn drop(&mut self) {
        crate::tui::theme::restore_tier(self.prev);
    }
}

/// A minimal `Profile` with every optional field unset — tests fill in what
/// they assert on.
pub(crate) fn blank_profile(name: &crate::profile::ProfileName) -> crate::profile::Profile {
    crate::profile::Profile {
        name: name.clone(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: Default::default(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// Write `entries` as a profile's `usage_history.jsonl` — the on-disk shape
/// `profile::load_usage_history` parses — so a fixture can give an account a
/// measured burn rate without running a fetch. Timestamps are `now_ms`-space
/// epoch milliseconds. The write resolves under the process HOME: the caller's
/// `HomeSandbox` (the HOME pin) is what keeps it off a real `~/.clauth`.
pub(crate) fn write_usage_history(
    name: &crate::profile::ProfileName,
    entries: &[(u64, crate::usage::UsageInfo)],
) {
    let path = crate::profile::profile_history_path(name).expect("history path");
    let Some(parent) = path.parent() else {
        return;
    };
    std::fs::create_dir_all(parent).expect("mkdir history dir");
    let mut body = String::new();
    for (ts, usage) in entries {
        let line = serde_json::json!({ "ts": ts, "name": name, "usage": usage });
        body.push_str(&line.to_string());
        body.push('\n');
    }
    std::fs::write(&path, body).expect("write history");
}

/// Put `names` in the on-disk profile list without creating profile content.
/// For fixtures that drive legs which re-read the record — the cache-write
/// gate, the acquire gate — but do not need per-profile files. Idempotent.
pub(crate) fn register_names(names: &[&str]) {
    let mut state = crate::profile::load_app_state().expect("load app state");
    for name in names {
        if !state.profiles.iter().any(|n| n == name) {
            state.profiles.push((*name).into());
        }
    }
    crate::profile::save_app_state(&state).expect("save app state");
}

// ── provider-cache fixtures ──────────────────────────────────────────────────
//
// A `third_party_cache.json` in each of the two SHAPES the provider legs really
// write. Both carry the complete key set, row set and row `kind`s read off an
// operator's own caches on 2026-08-15 (a DeepSeek balance profile and a z.ai bar
// profile, `serde_json` key-shape only); every number, amount and reset stamp is
// substituted, so these are shaped-from-a-capture rather than captured bytes,
// and no account figure is committed.
//
// Kept as BYTES rather than a serialized `ThirdPartyStats`: a struct built in
// Rust agrees with whatever the reader guessed, while these go through the
// production reader like every real consumer does.

/// The balance shape: `rows` only, no `bars`, and no `plan` key at all. Its
/// wallet row carries the CAPTURED `total`, which is also the label every cache
/// an older clauth wrote still holds on disk today, and the one the generic
/// scanner still passes an endpoint's own key through as. Consumers must keep
/// reading it — [`DEEPSEEK_CACHE_BYTES`] is the same shape at the current
/// spelling, and the two together are what hold both halves of that.
pub(crate) const THIRD_PARTY_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"CNY balance","value":"","kind":"heading"},{"label":"total","value":"31.45 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"31.45 CNY","kind":"body"}],"bars":[],"best_effort":false}"#;

/// [`THIRD_PARTY_CACHE_BYTES`] as the DeepSeek leg writes it now: same capture,
/// same key set, wallet row at [`crate::providers::DEEPSEEK_BALANCE_ROW_LABEL`].
/// For a test asserting what a DeepSeek account renders TODAY; the constant
/// above is what it renders off a cache written before the rename.
pub(crate) const DEEPSEEK_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"CNY balance","value":"","kind":"heading"},{"label":"api balance","value":"31.45 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"31.45 CNY","kind":"body"}],"bars":[],"best_effort":false}"#;

/// The same account after DeepSeek reports its balance cannot fund a call: the
/// wallets still arrive and `ThirdPartyStats::unfunded` appends the refusal, so
/// every surface can render the figure and the verdict together.
pub(crate) const DEEPSEEK_UNFUNDED_CACHE_BYTES: &str = r#"{"is_available":false,"rows":[{"label":"CNY balance","value":"","kind":"heading"},{"label":"api balance","value":"0.00 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"0.00 CNY","kind":"body"},{"label":"","value":"balance too low","kind":"danger"}],"bars":[],"best_effort":false}"#;

/// The third shape, and the one a bar-count reader gets wrong: a provider that
/// PUBLISHES usage windows answering with none of them. `alibaba::window_bar`
/// drops a window whose percentage the response omitted and both are optional,
/// so this is what a qwen account caches when neither arrived — plan and
/// subscription rows, an empty `bars`, and no wallet anywhere.
pub(crate) const ALIBABA_NO_BARS_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"subscription","value":"","kind":"heading"},{"label":"status","value":"valid","kind":"body"},{"label":"remaining","value":"84 days","kind":"body"}],"bars":[],"plan":"coding plan","best_effort":false}"#;

/// The bar shape: three `bars` under a `plan` label, of which only the longest
/// window carries `used`/`total`, plus the section-headed row set a token
/// provider writes. The mixed bar keys are the point — a reader that assumed
/// every bar carries the same five fields parses this one wrong.
pub(crate) const THIRD_PARTY_BARS_CACHE_BYTES: &str = r#"{"is_available":true,"rows":[{"label":"30d","value":"","kind":"heading"},{"label":"search-prime","value":"12 / 100","kind":"body"},{"label":"web-reader","value":"3 / 100","kind":"body"},{"label":"zread","value":"0 / 50","kind":"body"},{"label":"7d tokens","value":"","kind":"heading"},{"label":"GLM-5.3","value":"80.1M","kind":"body"},{"label":"GLM-5.2","value":"40.2M","kind":"body"},{"label":"GLM-4.7","value":"3.1M","kind":"body"},{"label":"total","value":"123.4M  (1.2k calls)","kind":"faint"}],"bars":[{"label":"5h","pct":12.5,"resets_at":"2026-08-15T12:00:00Z"},{"label":"7d","pct":48.0,"resets_at":"2026-08-20T00:00:00Z"},{"label":"30d","pct":3.0,"resets_at":"2026-09-01T00:00:00Z","used":123.4,"total":4000.0}],"plan":"pro","best_effort":false}"#;

/// A real two-wallet DeepSeek cache: the empty USD wallet FIRST, the funded
/// CNY wallet second. Captured 2026-08-28 from the operator's `DS6` profile's
/// `third_party_cache.json` (every figure verbatim), with the two wallet
/// blocks reordered to the USD-first order `DS8`'s cache held after its
/// 2026-08-17 17:59 fetch — the API's own `balance_infos` order is not stable,
/// an account can hold two wallets and the provider lists them in its own
/// order. No live cache held the empty-first order when this fixture was cut,
/// so the fixture is captured bytes written through the production writer,
/// never a hand-built `ThirdPartyStats`.
pub(crate) const CAPTURED_TWO_WALLET_DS_CACHE: &str = r#"{"is_available":true,"rows":[{"label":"USD balance","value":"","kind":"heading"},{"label":"api balance","value":"0.00 USD","kind":"body"},{"label":"granted","value":"0.00 USD","kind":"body"},{"label":"topped up","value":"0.00 USD","kind":"body"},{"label":"CNY balance","value":"","kind":"heading"},{"label":"api balance","value":"498.18 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"498.18 CNY","kind":"body"}],"bars":[],"best_effort":false}"#;

/// A real one-wallet DeepSeek cache, captured verbatim 2026-08-28 from the
/// operator's `D1` profile's `third_party_cache.json`. The one-wallet control
/// for the two-wallet ruling: nothing to drop, the figure must render exactly
/// as before.
pub(crate) const CAPTURED_ONE_WALLET_DS_CACHE: &str = r#"{"is_available":true,"rows":[{"label":"CNY balance","value":"","kind":"heading"},{"label":"api balance","value":"3640.55 CNY","kind":"body"},{"label":"granted","value":"0.00 CNY","kind":"body"},{"label":"topped up","value":"3640.55 CNY","kind":"body"}],"bars":[],"best_effort":false}"#;

/// Parse a captured `third_party_cache.json` and write it at `name`'s
/// sandboxed path through the production cache writer — the same route the
/// fetch leg takes — so a consumer is driven by captured bytes, never a
/// hand-built [`crate::providers::ThirdPartyStats`] that mirrors the reader's
/// own guess.
pub(crate) fn write_captured_third_party_cache(name: &str, json: &str) {
    let parsed: crate::providers::ThirdPartyStats =
        serde_json::from_str(json).expect("captured cache parses");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &parsed,
    );
}

/// A live-session registry row with every field a fixture rarely varies already
/// filled: one non-isolated, chain-following session of `profile` that has never
/// swapped. Callers override the fields their case is actually about.
pub(crate) fn live_row(session_id: &str, profile: &str) -> crate::live_sessions::LiveSession {
    crate::live_sessions::LiveSession {
        session_id: session_id.to_owned(),
        start_profile: profile.to_owned(),
        pid: 4242,
        started_at: 1_700_000_000_000,
        cwd: None,
        isolated: false,
        follows_chain: true,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
    }
}

/// Overwrite a file's modification time — for cache-staleness / tie-break tests.
///
/// The open retries while Windows reports a sharing violation: an open landing
/// inside another thread's `MoveFileEx` replace over the same path fails with
/// it, which is exactly what a fixture that back-dates a file the code under
/// test is concurrently republishing does. POSIX renames never block an open,
/// so `is_sharing_violation` is `false` off Windows and the loop degenerates to
/// one attempt. Bounded, so a genuinely absent file still panics with its own
/// error rather than hanging.
pub(crate) fn set_mtime(path: &Path, when: SystemTime) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let file = loop {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(file) => break file,
            Err(e) if is_sharing_violation(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("open {} for mtime: {e}", path.display()),
        }
    };
    file.set_modified(when).expect("set_modified");
}

/// `ERROR_SHARING_VIOLATION`. `std::io::ErrorKind` maps it to `Uncategorized`,
/// so the raw code is the only discriminator, and it is Windows-only: errno 32
/// is `EPIPE` on Linux.
///
/// It names what an OPEN gets. A rename replace over a destination someone else
/// holds open fails `ERROR_ACCESS_DENIED` (5) instead, measured on a real box,
/// so this predicate does not carry to a publish.
#[cfg(windows)]
fn is_sharing_violation(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(32)
}

#[cfg(not(windows))]
fn is_sharing_violation(_: &std::io::Error) -> bool {
    false
}

/// A `Press` key event with no modifiers.
pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Collect a `Command`'s queued env overrides: key → `Some(value)` for a set
/// var, key → `None` for a removed one. `get_envs` reflects only the explicit
/// `env`/`env_remove` ops, which is exactly what we assert. No process env or
/// spawn needed, so this is lock-free and non-flaky.
pub(crate) fn env_overrides(cmd: &Command) -> HashMap<String, Option<String>> {
    cmd.get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|s| s.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

/// Every path under `root` breaking the owner-only invariant clauth holds over
/// `~/.clauth` (0o700 dirs, 0o600 files), rendered as `<mode> <path>` lines.
/// Symlinks are skipped — a link's own mode is meaningless and its target lives
/// outside the tree.
#[cfg(unix)]
pub(crate) fn owner_only_violations(root: &Path) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;

    let mut out = Vec::new();
    let Ok(meta) = root.symlink_metadata() else {
        return out;
    };
    if meta.file_type().is_symlink() {
        return out;
    }
    let is_dir = meta.file_type().is_dir();
    let mode = meta.permissions().mode() & 0o777;
    let want = if is_dir { 0o700 } else { 0o600 };
    if mode != want {
        out.push(format!("{mode:#o} {} (want {want:#o})", root.display()));
    }
    if is_dir && let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            out.extend(owner_only_violations(&entry.path()));
        }
    }
    out
}

/// Flatten a rendered `TestBackend` buffer to one `String` per row (cell symbols
/// concatenated). Shared by the TUI render tests so each keeps a single copy of
/// the buffer→text step; callers `.concat()` or `.join("\n")` to taste.
pub(crate) fn buffer_rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    let w = buf.area.width as usize;
    let h = buf.area.height as usize;
    (0..h)
        .map(|y| (0..w).map(|x| buf.content[y * w + x].symbol()).collect())
        .collect()
}

// A fake `claude` on a PATH prefix whose final-run invocation holds the child
// alive for a few seconds, so a test can read a session's runtime
// `settings.json` MID-run: `ProfileRuntime`'s drop removes the tree once the
// child exits, so nothing written there survives to be asserted afterwards.

/// The poll helper: wait until a runtime settings.json appears under
/// `profile_dir`, then return its content. Bounded so a fixture that never
/// merges fails the test instead of hanging the suite.
#[cfg(unix)]
pub(crate) fn runtime_settings_until(profile_dir: &std::path::Path) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let mut found = None;
        if let Ok(entries) = std::fs::read_dir(profile_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("runtime-") {
                    let settings = entry.path().join("settings.json");
                    if settings.is_file() {
                        found = Some(settings);
                        break;
                    }
                }
            }
        }
        if let Some(settings) = found {
            return std::fs::read_to_string(settings).ok();
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A stateful slow `claude` shim: `--version` and `plugin` probes answer
/// instantly (the start pre-flight's heal calls them), the session spawn
/// itself sleeps a bounded five seconds so the poll above can observe the
/// tree, then exits 0. Mutates process-global env, so it borrows the
/// [`HomeSandbox`] whose `HOME_TEST_LOCK` serializes every other env pin in
/// the suite — the same shape `FakeClaude` uses; this one differs only in
/// keeping the final child alive.
#[cfg(unix)]
pub(crate) struct SlowClaude<'a> {
    _home: std::marker::PhantomData<&'a HomeSandbox>,
    _tmp: tempfile::TempDir,
    prev_path: std::ffi::OsString,
    prev_home: Option<std::ffi::OsString>,
    prev_data: Option<std::ffi::OsString>,
    prev_runtime: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl<'a> SlowClaude<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    pub(crate) fn new(home: &'a HomeSandbox) -> Self {
        let tmp = tempfile::tempdir_in(home.home()).expect("shim dir");
        let shim = tmp.path().join("claude");
        std::fs::write(
            &shim,
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"2.1.220 (Claude Code)\"; exit 0;;\n  plugin) exit 0;;\nesac\nsleep 5\nexit 0\n",
        )
        .expect("write shim");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim).expect("shim meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).expect("chmod shim");

        let prev_path = std::env::var_os("PATH").expect("PATH is set");
        let mut path = std::ffi::OsString::from(tmp.path());
        path.push(":");
        path.push(&prev_path);
        // The `dirs`-crate pins keep agentgear's data/runtime resolution off
        // the operator's real dirs, exactly like `FakeClaude::stage`.
        let pin = |key: &str, value: &std::path::Path| {
            let prev = std::env::var_os(key);
            // SAFETY: test-only, serialized by HOME_TEST_LOCK, restored on drop.
            unsafe { std::env::set_var(key, value) };
            prev
        };
        // SAFETY: test-only, serialized by HOME_TEST_LOCK, restored on drop.
        unsafe { std::env::set_var("PATH", path) };
        let prev_data = pin("XDG_DATA_HOME", &tmp.path().join("data"));
        let prev_home = pin("HOME", home.home());
        let prev_runtime = pin("XDG_RUNTIME_DIR", &tmp.path().join("run"));
        Self {
            _home: std::marker::PhantomData,
            _tmp: tmp,
            prev_path,
            prev_home,
            prev_data,
            prev_runtime,
        }
    }
}

#[cfg(unix)]
impl Drop for SlowClaude<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, still held here"
    )]
    fn drop(&mut self) {
        // SAFETY: restore the prior values under the same lock the sandbox holds.
        unsafe {
            std::env::set_var("PATH", &self.prev_path);
            for (key, value) in [
                ("XDG_DATA_HOME", &self.prev_data),
                ("HOME", &self.prev_home),
                ("XDG_RUNTIME_DIR", &self.prev_runtime),
            ] {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
