//! Shared test-only helpers used across the inline test modules
//! (`tests/inline/*.rs`). Defined once here rather than copied per module so the
//! home-sandbox, mtime, and key-event scaffolding stays in a single place.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use utoipa::ToSchema;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{
    AdditionalProperties, Array, ArrayItems, Components, Object, Schema, SchemaType, Type,
};

/// Every `.rs` file under `dir`, recursively: the walk the source-scan tests
/// share (`out::tests::no_bare_print_macro_under_src`, the Keychain budget
/// site scan). An unreadable dir yields nothing, so each caller asserts it
/// read the crate.
pub(crate) fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rs_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

/// `path` canonicalized and spelled as a plain path is shown or handed to
/// another program to open, derived from dunce's rule rather than by calling
/// it: on Windows a `\\?\X:\…` verbatim path drops `\\?\` while the whole
/// verbatim string is at most 260 UTF-16 units; a longer one and a
/// `\\?\UNC\…` one keep it. (dunce also keeps a path holding a reserved DOS
/// name; no test builds one.) Elsewhere canonicalize adds no prefix.
pub(crate) fn plain_path(path: &Path) -> PathBuf {
    let resolved = crate::canonical::identity(path).expect("canonical");
    let text = resolved.to_str().expect("utf-8 path");
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"UNC\") && text.encode_utf16().count() <= 260 => {
            PathBuf::from(rest)
        }
        _ => resolved,
    }
}

/// `path` canonicalized and spelled as Claude Code keys its project dirs on
/// Windows: `\\?\` dropped at any length, `\\?\UNC\server\…` as
/// `\\server\…`. Elsewhere canonicalize adds no prefix.
pub(crate) fn node_path(path: &Path) -> PathBuf {
    let resolved = crate::canonical::identity(path).expect("canonical");
    let text = resolved.to_str().expect("utf-8 path");
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        resolved
    }
}

/// Downcast a feed fetch's failure to ureq's own timeout and pin its REASON:
/// only `Global` is the one end-to-end budget cloudy's 2026-10-21 ruling names.
/// A `RecvBody` bound or one of ureq's other phases is a different, unruled
/// budget even when it errors inside the window.
///
/// ureq 3.4.2's `BodyReader` surfaces its timeout as `io::Error::other`, so the
/// ureq error is the io error's inner source.
pub(crate) fn assert_one_budget_timeout<T: std::fmt::Debug>(result: &anyhow::Result<T>) {
    let err = result.as_ref().expect_err("a stalled body is an error");
    let io = err
        .downcast_ref::<std::io::Error>()
        .unwrap_or_else(|| panic!("the body stall surfaces an io error: {err:?}"));
    let ureq_err = io
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
        .unwrap_or_else(|| panic!("the io error wraps ureq's timeout: {err:?}"));
    assert!(
        matches!(ureq_err, ureq::Error::Timeout(ureq::Timeout::Global)),
        "the body stall must trip the ONE end-to-end budget: {err:?}"
    );
}

/// Read a loopback stub's request head before it answers, as a real server
/// does. A socket closed with the request still unread sends a RST instead of a
/// FIN: Windows then fails the client's read with `os error 10053`, and macOS
/// refuses the client's next `SO_RCVTIMEO` with `EINVAL` (CI run 39758751460).
pub(crate) fn read_request_head(sock: &mut std::net::TcpStream) {
    use std::io::Read;
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
}

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
    prev_session_id: Option<std::ffi::OsString>,
    prev_claude_pid: Option<std::ffi::OsString>,
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
        // A prior test that panicked mid-override leaks it into this one: clear
        // it here (and on drop) so a parallel test never inherits a shortened
        // lock deadline it did not set. The callback/feed bounds and the
        // marker-registration pose are the same class of process-global state.
        crate::lock::set_state_lock_timeout_override(None);
        set_tui_worker_join_timeout_override(None);
        crate::oauth_login::set_callback_read_timeout_override(None);
        crate::oauth_login::set_callback_reader_cap_override(None);
        crate::codex_login::set_callback_read_timeout_override(None);
        crate::pricing::set_fetch_timeout_override(None);
        crate::status::set_fetch_timeout_override(None);
        crate::mcp::jobs::reset_marker_registration_for_test();
        let tmp = tempfile::tempdir().expect("create home sandbox");
        let home = tmp.path().to_path_buf();
        crate::profile::set_home_override(home.clone());
        // `home_override` does not reach `CLAUDE_CONFIG_DIR`, and the user's
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
        // Same for the live Claude Code session the suite may run under: its
        // session id and process would stamp a minted delegate's host and decide
        // a hook's own process, passing or failing by where the suite ran.
        let prev_session_id = std::env::var_os("CLAUDE_CODE_SESSION_ID");
        let prev_claude_pid = std::env::var_os("CLAUDE_PID");
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("CLAUDE_CODE_SESSION_ID");
            std::env::remove_var("CLAUDE_PID");
        }
        Self {
            _tmp: tmp,
            _guard: guard,
            home,
            prev_config_dir,
            prev_session_id,
            prev_claude_pid,
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
        // worker still running past the join bound is detached, so its next
        // home read resolves whichever `HOME_OVERRIDE` is installed then — a
        // later test's sandbox under one-process `cargo test` — and never the
        // operator's real `$HOME` (a `cfg(test)` home read with no override
        // panics inside the worker's own `catch_unwind`). Doing it here rather
        // than asking each test to call the join fns covers the tests that
        // never thought about it — which is every test that will ever be
        // added. Two registries: the TUI's own `spawn_worker` handles
        // (joinable OS threads) and [`join_background_tasks`] for detach
        // mechanisms that hand back no joinable handle at all (e.g.
        // `tokio::task::spawn_blocking`).
        let stuck = join_tui_workers_bounded();
        join_background_tasks();
        // Clear the forget queue, its registered-stores flag and the seed
        // hooks: under `cargo test` the suite is one process, so a leak or an
        // armed hook would reach the next test.
        crate::usage::reset_pending_forgets_for_test();
        crate::usage::clear_seed_hooks_for_test();
        crate::usage::clear_codex_compare_hooks_for_test();
        crate::profile::clear_home_override();
        // Clear the process-wide lock-timeout override after the joins above
        // (a worker's last wait still needs the shortened deadline it was
        // posed under), so a test that forgot to clear it cannot leak it into
        // the next one. The callback/feed bounds and the marker-registration
        // pose leak the same way, so they clear together.
        crate::lock::set_state_lock_timeout_override(None);
        set_tui_worker_join_timeout_override(None);
        crate::oauth_login::set_callback_read_timeout_override(None);
        crate::oauth_login::set_callback_reader_cap_override(None);
        crate::codex_login::set_callback_read_timeout_override(None);
        crate::pricing::set_fetch_timeout_override(None);
        crate::status::set_fetch_timeout_override(None);
        crate::mcp::jobs::reset_marker_registration_for_test();
        // Restore the user's own value, after the joins above for the same
        // reason the home override is cleared there: a still-running worker that
        // sees it back resolves outside the sandbox.
        // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, still held.
        match self.prev_config_dir.take() {
            Some(prev) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", prev) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
        match self.prev_session_id.take() {
            Some(prev) => unsafe { std::env::set_var("CLAUDE_CODE_SESSION_ID", prev) },
            None => unsafe { std::env::remove_var("CLAUDE_CODE_SESSION_ID") },
        }
        match self.prev_claude_pid.take() {
            Some(prev) => unsafe { std::env::set_var("CLAUDE_PID", prev) },
            None => unsafe { std::env::remove_var("CLAUDE_PID") },
        }
        // A non-panicking miss ran the whole teardown above; panic LAST naming
        // the count and the bound. The unwind then drops the guard (poisoning
        // `HOME_TEST_LOCK`, which the next sandbox recovers) and the tempdir.
        if let Some(msg) = stuck {
            panic!("{msg}");
        }
    }
}

/// Run a switch fn (`switch_profile`/`switch_off`/`auto_switch_if_needed`, all
/// [`crate::profile::ConfigHandle`]-taking) over an owned `AppConfig` and hand
/// the mutated value back. The fns lock internally and run their post-switch
/// feed republish on the handle, so the test moves the value in and clones it
/// out after — every later assert reads the post-switch state.
pub(crate) fn through_handle<T>(
    config: crate::profile::AppConfig,
    run: impl FnOnce(&crate::profile::ConfigHandle) -> T,
) -> (crate::profile::AppConfig, T) {
    let handle: crate::profile::ConfigHandle =
        std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
    let out = run(&handle);
    let config = handle.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (config, out)
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

/// The bound [`HomeSandbox::drop`] waits for TUI workers: [`BACKGROUND_TASK_JOIN_TIMEOUT`],
/// or a shorter value a test poses a hung worker under. Production never sets
/// the override. Reset by `HomeSandbox` (which serializes the whole suite
/// through `HOME_TEST_LOCK`), so a parallel test never sees a leaked override.
#[cfg(test)]
static TUI_WORKER_JOIN_TIMEOUT_OVERRIDE: std::sync::Mutex<Option<std::time::Duration>> =
    std::sync::Mutex::new(None);

/// The bound [`HomeSandbox::drop`] waits for TUI workers: the test-only
/// override when set, else [`BACKGROUND_TASK_JOIN_TIMEOUT`].
#[cfg(test)]
fn tui_worker_join_timeout() -> std::time::Duration {
    if let Some(t) = *TUI_WORKER_JOIN_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return t;
    }
    BACKGROUND_TASK_JOIN_TIMEOUT
}

/// Set or clear the test-only TUI-worker join bound. `None` restores
/// [`BACKGROUND_TASK_JOIN_TIMEOUT`].
#[cfg(test)]
pub(crate) fn set_tui_worker_join_timeout_override(timeout: Option<std::time::Duration>) {
    *TUI_WORKER_JOIN_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout;
}

/// Join every TUI worker bounded, so a worker that never finishes cannot turn a
/// test's teardown into a hung gate. On a miss the stuck workers are detached,
/// and a detached worker's next home read resolves whichever `HOME_OVERRIDE` is
/// installed then — never the operator's real `$HOME`. A panicking miss logs
/// and returns `None`; a non-panicking miss returns the message for
/// [`HomeSandbox::drop`] to panic with LAST, after the rest of the teardown.
#[cfg(test)]
fn join_tui_workers_bounded() -> Option<String> {
    let bound = tui_worker_join_timeout();
    if let Err(n) = crate::tui::join_test_workers_within(bound) {
        let suffix = if n == 1 { "" } else { "s" };
        let msg = format!("{n} TUI worker{suffix} still running past the {bound:?} join bound");
        crate::tui::detach_test_workers();
        if std::thread::panicking() {
            crate::out::errln!(
                "clauth: {msg}; detached, its next home read may resolve the next \
                 sandboxed test's home, never the real one"
            );
            None
        } else {
            Some(msg)
        }
    } else {
        None
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
    let (base, inner) = serve_endpoints_recording(max, reply);
    let handle = std::thread::spawn(move || {
        inner
            .join()
            .expect("recording listener")
            .into_iter()
            .map(|(path, _body)| path)
            .collect()
    });
    (base, handle)
}

/// [`serve_endpoints`] that also hands back each request's BODY, for a leg
/// whose correctness is in what it sent (the paste door's `redirect_uri` and
/// `state`) rather than in which endpoint it reached. Same listener, same
/// deadlines; `serve_endpoints` is a projection of this one, and this one of
/// [`serve_endpoints_raw`].
pub(crate) fn serve_endpoints_recording(
    max: usize,
    reply: impl Fn(&str, usize) -> (u16, String) + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
    let (base, inner) = serve_endpoints_raw(max, reply);
    let handle = std::thread::spawn(move || {
        inner
            .join()
            .expect("raw listener")
            .into_iter()
            .map(|raw| (request_path(&raw), request_body(&raw)))
            .collect()
    });
    (base, handle)
}

/// The request path off a raw request text, as the listener saw it.
pub(crate) fn request_path(raw: &str) -> String {
    raw.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string()
}

/// The body off a raw request text: everything past the header terminator.
pub(crate) fn request_body(raw: &str) -> String {
    raw.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default()
}

/// One header's value off a raw request text, matched case-insensitively the
/// way a server reads it; `None` when the request never sent it.
pub(crate) fn request_header(raw: &str, name: &str) -> Option<String> {
    raw.split_once("\r\n\r\n")
        .map_or(raw, |(head, _)| head)
        .lines()
        .skip(1)
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
}

/// Read an ENTIRE HTTP request (headers + any `Content-Length` body) off a
/// stream before the caller writes its response. A close that leaves request
/// bytes unread sends RST instead of FIN, and an RST can discard the client's
/// not-yet-read response. A bodyless GET drains in one read, which is why only
/// the POST-driven servers need this.
pub(crate) fn drain_http_request(sock: &mut impl std::io::Read) -> Vec<u8> {
    let mut req = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match sock.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                req.extend_from_slice(&tmp[..n]);
                if let Some(hlen) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let cl = req[..hlen]
                        .split(|&b| b == b'\n')
                        .find_map(|line| {
                            std::str::from_utf8(line)
                                .ok()?
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if req.len() >= hlen + 4 + cl {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    req
}

/// The projection under [`serve_endpoints_recording`]: hands back each
/// request's RAW text, headers included, for a leg whose correctness is in a
/// header it sent (a bearer token, an account id, a content type). It is
/// [`serve_endpoints_raw_with_headers`] replying with no extra headers, so the
/// same listener and deadlines.
pub(crate) fn serve_endpoints_raw(
    max: usize,
    reply: impl Fn(&str, usize) -> (u16, String) + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    serve_endpoints_raw_with_headers(max, move |path, i| {
        let (status, body) = reply(path, i);
        (status, Vec::new(), body)
    })
}

/// [`serve_endpoints_raw`] whose replies also carry response headers, for a
/// leg that reads what the server said beside the status (the limiter's
/// rate-limit headers on a kick 429). Same listener, same deadlines.
pub(crate) fn serve_endpoints_raw_with_headers(
    max: usize,
    reply: impl Fn(&str, usize) -> (u16, Vec<(String, String)>, String) + Send + 'static,
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
            let (status, headers, body) = reply(&request_path(&text), i);
            let extra: String = headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}\r\n"))
                .collect();
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = sock.write_all(body.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Write);
            seen.push(text);
        }
        seen
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// The rate-limit header block of a real `/v1/messages` 429 from an exhausted
/// 5h window, in its captured order (2026-09-24 on a Max 5x account:
/// `5h-utilization` 1.0, `representative-claim` `five_hour`; a 2026-07-14 Pro
/// capture agrees), with its two epochs supplied by the caller. The capture sent
/// `reset` 6073 s and `weekly_reset` 383473 s past its `Date`, beside
/// `retry-after: 6072`.
pub(crate) fn window_exhaustion_429_headers(
    reset: i64,
    weekly_reset: i64,
) -> Vec<(String, String)> {
    let (reset, weekly_reset) = (reset.to_string(), weekly_reset.to_string());
    [
        ("x-should-retry", "true"),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour",
        ),
        ("anthropic-ratelimit-unified-upgrade-paths", "upgrade_plan"),
        (
            "anthropic-ratelimit-unified-7d-reset",
            weekly_reset.as_str(),
        ),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("retry-after", "6072"),
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-reset", reset.as_str()),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        ("anthropic-ratelimit-unified-5h-surpassed-threshold", "1.0"),
        ("anthropic-ratelimit-unified-5h-reset", reset.as_str()),
        (
            "anthropic-ratelimit-unified-overage-disabled-reason",
            "org_level_disabled",
        ),
        ("anthropic-ratelimit-unified-fallback-percentage", "0.5"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.62"),
        ("anthropic-ratelimit-unified-status", "rejected"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// A listener bound but never accepted, and its `http://127.0.0.1:PORT` base:
/// a request would queue a connection on it, so `accept` answering
/// `WouldBlock` afterwards ([`assert_nothing_sent`]) proves nothing was sent —
/// at once, with no stub deadline to wait out.
pub(crate) fn deaf_listener() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    (listener, format!("http://127.0.0.1:{port}"))
}

pub(crate) fn assert_nothing_sent(listener: &std::net::TcpListener) {
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("a request reached the endpoint: {other:?}"),
    }
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
            ..crate::profile::OAuthToken::default_extra()
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

/// Make `dir` refuse to create or write files for the guard's lifetime, so a
/// test can drive a save failure without root: on Unix the dir's mode drops to
/// 0o500 (no write bit), on Windows a deny ACE for Everyone removes add-file /
/// add-subdir (`(WD,AD)`). The constructor probes a real write so a fault that
/// never bites fails by name instead of greening the test; `Drop` restores
/// best-effort and never asserts.
#[cfg(test)]
pub(crate) struct UnwritableDir {
    path: PathBuf,
}

#[cfg(test)]
impl UnwritableDir {
    pub(crate) fn new(path: &Path) -> Self {
        let guard = Self {
            path: path.to_path_buf(),
        };
        guard.arm();
        guard.probe();
        guard
    }

    #[cfg(unix)]
    fn arm(&self) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o500))
            .unwrap_or_else(|e| {
                panic!(
                    "UnwritableDir: chmod 0o500 {} failed: {e}",
                    self.path.display()
                )
            });
    }

    #[cfg(windows)]
    fn arm(&self) {
        let out = Command::new(icacls())
            .arg(&self.path)
            .arg("/deny")
            .arg("*S-1-1-0:(WD,AD)")
            .output()
            .unwrap_or_else(|e| panic!("UnwritableDir: spawn icacls failed: {e}"));
        if !out.status.success() {
            panic!(
                "UnwritableDir: icacls /deny {} failed: {}",
                self.path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn probe(&self) {
        let probe = self
            .path
            .join(format!(".unwritable-probe-{}", std::process::id()));
        match std::fs::write(&probe, b"probe") {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                panic!(
                    "UnwritableDir({}) accepted a probe write",
                    self.path.display()
                );
            }
            Err(e) => panic!(
                "UnwritableDir({}) probe: expected PermissionDenied, got {e}",
                self.path.display()
            ),
        }
    }
}

#[cfg(test)]
impl Drop for UnwritableDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(windows)]
        {
            let _ = Command::new(icacls())
                .arg(&self.path)
                .arg("/remove:d")
                .arg("*S-1-1-0")
                .output();
        }
    }
}

#[cfg(windows)]
fn icacls() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("icacls.exe")
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

/// RAII pin pointing the codex token endpoint — the wire behind
/// `codex_auth::refresh_codex_chain`, which `standby_tick` hardwires — at
/// `base`, cleared on drop even if the test panics. Borrows the
/// [`HomeSandbox`] for the reason [`EndpointSandbox`] does: the override is a
/// process-global serialized by `HOME_TEST_LOCK`, and a fixture panic between
/// two plain set/clear calls would leave it pointing the next test at a dead
/// port.
pub(crate) struct CodexTokenUrlSandbox<'a>(std::marker::PhantomData<&'a HomeSandbox>);

impl<'a> CodexTokenUrlSandbox<'a> {
    pub(crate) fn new(_home: &'a HomeSandbox, base: &str) -> Self {
        crate::codex_auth::set_token_url_override(&format!("{base}/oauth/token"));
        Self(std::marker::PhantomData)
    }
}

impl Drop for CodexTokenUrlSandbox<'_> {
    fn drop(&mut self) {
        crate::codex_auth::clear_token_url_override();
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
    prevs: Vec<(String, Option<std::ffi::OsString>)>,
    _home: std::marker::PhantomData<&'a HomeSandbox>,
}

impl<'a> EnvPin<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    pub(crate) fn new(_home: &'a HomeSandbox, pins: &[(&str, Option<&std::ffi::OsStr>)]) -> Self {
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
            prevs.push((key.to_string(), prev));
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
printf '%s\t%s\n' "${CLAUDE_CONFIG_DIR-}" "$*" >> "$CLAUDE_SHIM_LOG.spawns"
# Like the real background launch: the job service takes the session, the
# launch returns at once, and the job record names the config dir it reads.
# The record is written through that config dir, as Claude Code writes it.
for arg in "$@"; do
  case "$arg" in
    --bg|--background)
      : "${CLAUDE_CONFIG_DIR:?}"
      job="$CLAUDE_CONFIG_DIR/jobs/fakebg$$"
      mkdir -p "$job"
      printf '{"state":"working","respawnFlags":[],"providerEnv":{"CLAUDE_CONFIG_DIR":"%s"},"sessionId":"fakebg%s-0a6e-4b11-883e-fca3a218ae52"}\n' "$CLAUDE_CONFIG_DIR" "$$" > "$job/state.json"
      mkdir -p "$CLAUDE_CONFIG_DIR/projects/-fake-cwd"
      printf '{"type":"user"}\n' > "$CLAUDE_CONFIG_DIR/projects/-fake-cwd/fakebg$$.jsonl"
      exit 0
      ;;
  esac
done
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
        # the Services tab recompute after the install sees it.
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

    /// Every call the shim answered, as `(CLAUDE_CONFIG_DIR, argv)`, in call
    /// order; an unset `CLAUDE_CONFIG_DIR` reads as the empty string.
    pub(crate) fn spawns(&self) -> Vec<(String, String)> {
        let mut path = self.log.clone().into_os_string();
        path.push(".spawns");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(dir, argv)| (dir.to_string(), argv.to_string()))
            .collect()
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

/// RAII `CODEX_HOME` override — [`ConfigDirSandbox`]'s codex twin, same lock
/// discipline, same restore-on-drop.
pub(crate) struct CodexHomeSandbox<'a> {
    prev: Option<std::ffi::OsString>,
    _home: std::marker::PhantomData<&'a HomeSandbox>,
}

impl<'a> CodexHomeSandbox<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    pub(crate) fn new(_home: &'a HomeSandbox, dir: &Path) -> Self {
        let prev = std::env::var_os("CODEX_HOME");
        // SAFETY: test-only, serialized by `HOME_TEST_LOCK`, restored on drop.
        unsafe { std::env::set_var("CODEX_HOME", dir) };
        Self {
            prev,
            _home: std::marker::PhantomData,
        }
    }
}

impl Drop for CodexHomeSandbox<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn drop(&mut self) {
        // SAFETY: same as `new` — restore the prior value under the same lock.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
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

/// RAII tier pin: forces the thread-local color tier for its lifetime, putting
/// the previous pin back on drop (even on panic). Required for a test asserting
/// a non-Full tier's style; in test builds the tier is per-thread and defaults
/// to `Full`, so an unsandboxed Full-tier assertion needs no pin. `!Send` by
/// construction: a pin is a claim on the thread that installed it, and one
/// carried to another thread would restore that thread's empty slot and leave
/// the installing thread pinned.
pub(crate) struct TierSandbox {
    prev: Option<crate::tui::theme::Tier>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl TierSandbox {
    pub(crate) fn new(tier: crate::tui::theme::Tier) -> Self {
        let prev = crate::tui::theme::tier_override();
        crate::tui::theme::set_tier(tier);
        Self {
            prev,
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for TierSandbox {
    fn drop(&mut self) {
        crate::tui::theme::restore_tier(self.prev);
    }
}

/// A section header's foreground on `Tier::Full` while focus rests in its
/// section: `TEXT` (Catppuccin Mocha `text`, `#cdd6f4`). Literal, so a theme
/// edit cannot move the expectation with it.
pub(crate) const EYEBROW_ACTIVE: ratatui::style::Color = ratatui::style::Color::Rgb(205, 214, 244);
/// A section header's foreground on `Tier::Full` at rest: `TEXT_DIM` (Mocha
/// `subtext0`, `#a6adc8`).
pub(crate) const EYEBROW_AT_REST: ratatui::style::Color = ratatui::style::Color::Rgb(166, 173, 200);

/// A minimal `Profile` with every optional field unset — tests fill in what
/// they assert on.
pub(crate) fn blank_profile(name: &crate::profile::ProfileName) -> crate::profile::Profile {
    crate::profile::Profile {
        name: name.clone(),
        base_url: None,
        api_key: None,
        auto_start: false,
        usage_probe: false,
        usage_probe_interval_ms: crate::profile::DEFAULT_USAGE_PROBE_INTERVAL_MS,
        env: Default::default(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        preferred_days: Vec::new(),
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        check_credit_cap: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        has_long_lived_sidecar: false,
        provider: None,
    }
}

/// A stored rotating OAuth pair (`credentials.json`'s shape) whose access
/// token is `access` — a profile holding one holds a login.
pub(crate) fn oauth_pair(access: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("{access}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// Write a long-lived `session-token.json` sidecar (no refresh token, so
/// `claude::has_session_token` reads it as a login) into `name`'s profile dir
/// under the process HOME, which the caller's `HomeSandbox` pins.
pub(crate) fn write_long_lived_sidecar(name: &crate::profile::ProfileName) {
    let mut sidecar = oauth_pair(&format!("sk-ant-oat-{name}"));
    if let Some(token) = sidecar.claude_ai_oauth.as_mut() {
        token.refresh_token = None;
    }
    let dir = crate::profile::profile_dir(name).expect("profile dir");
    std::fs::create_dir_all(&dir).expect("mkdir profile dir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("serialize sidecar"),
    )
    .expect("write session-token sidecar");
}

/// Make `p` the account the usage probe reads: the switch on and a long-lived
/// setup token where a fixture's pair was, in memory and in the committed store
/// a swap re-checks its target's login against. Runs under the caller's
/// `HomeSandbox`.
pub(crate) fn make_probed_account(p: &mut crate::profile::Profile) {
    p.usage_probe = true;
    p.credentials = None;
    p.has_long_lived_sidecar = true;
    crate::profile::save_profile(p).expect("save the probed profile");
    write_long_lived_sidecar(&p.name);
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

/// A JWT carrying `payload` (a JSON object) — header.payload.signature in the
/// base64url alphabet, signed by nobody: every clauth read of a codex token is
/// unverified, so this is all a schedule or label read needs.
pub(crate) fn codex_jwt(payload: &str) -> String {
    let payload = crate::oauth_login::base64url_nopad(payload.as_bytes());
    format!("h.{payload}.sig")
}

/// [`codex_jwt`] whose payload carries `exp` (epoch seconds) alone.
pub(crate) fn jwt_with_exp(exp_secs: i64) -> String {
    codex_jwt(&format!("{{\"exp\":{exp_secs}}}"))
}

/// A codex `auth.json` body holding one chain plus a key clauth never writes,
/// so a rotation's key survival is observable.
pub(crate) fn codex_auth_body(access: &str, refresh: &str) -> String {
    format!(
        "{{ \"tokens\": {{\"id_token\": \"id.x\", \"access_token\": \"{access}\", \
         \"refresh_token\": \"{refresh}\", \"account_id\": \"acc\"}}, \"keep_me\": 7 }}"
    )
}

/// Write `body` as `name`'s profile store (`profiles/<name>/auth.json`) under
/// the caller's [`HomeSandbox`].
pub(crate) fn write_codex_store(name: &str, body: &str) {
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    std::fs::write(dir.join("auth.json"), body).expect("write store");
}

pub(crate) fn read_codex_store(name: &str) -> String {
    std::fs::read_to_string(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("dir")
            .join("auth.json"),
    )
    .expect("read store")
}

/// Writes `body` verbatim as the sandboxed `~/.clauth/codex-profiles.toml`.
pub(crate) fn write_codex_state(body: &str) {
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir .clauth");
    std::fs::write(dir.join("codex-profiles.toml"), body).expect("write codex state");
}

/// Writes a roster-only `codex-profiles.toml` into the sandboxed `~/.clauth`.
pub(crate) fn write_codex_roster(names: &[&str]) {
    let list = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    write_codex_state(&format!("profiles = [{list}]\n"));
}

/// Warm codex `name`'s usage cache in the shape the codex leg writes: the plan
/// in `codex_plan` (when `plan` is `Some`), never `tier`. The codex roster must
/// already hold `name` — `warm_usage` would put it on the claude roster instead.
pub(crate) fn warm_codex_usage(name: &str, plan: Option<&str>, five_h: f64, seven_d: f64) {
    let usage = crate::usage::UsageInfo {
        plan: plan.map(|plan| crate::usage::PlanInfo {
            codex_plan: Some(plan.to_string()),
            ..crate::usage::PlanInfo::default()
        }),
        five_hour: Some(crate::usage::UsageWindow {
            utilization: five_h,
            resets_at: None,
        }),
        seven_day: Some(crate::usage::UsageWindow {
            utilization: seven_d,
            resets_at: None,
        }),
        fetched_at: Some(crate::usage::now_ms()),
        ..Default::default()
    };
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::USAGE_CACHE_FILE,
        &usage,
    );
}

/// Another clauth process inside its state-flock hold: a second open file
/// description of the lock file, flocked. Call under a [`HomeSandbox`]; drop it
/// to release.
pub(crate) fn hold_state_flock() -> std::fs::File {
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");
    holder
}

/// A locked handle on `name`'s rotation lock from a separate fd, standing in
/// for another process mid-rotation (`flock(2)` binds to the open file
/// description, so this genuinely contends with `try_acquire`'s own). Creates
/// the locks directory the way `RotationGuard::open` does, since a real holder
/// made it on its way in. Call under a [`HomeSandbox`]; drop it to release.
pub(crate) fn hold_rotation_lock(name: &str) -> std::fs::File {
    let path = crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from(name))
        .expect("rotation lock path");
    crate::profile::mkdir_700(path.parent().expect("lock parent")).expect("locks dir");
    let holder = crate::profile::open_state_file(&path).expect("open holder handle");
    holder.lock().expect("hold the rotation lock");
    holder
}

/// Simulate a live `clauth start` session for `name`: a locked pid file in the
/// profile's sessions dir under `home` reads as alive via
/// `runtime::has_live_session`. The caller must keep the returned file alive for
/// as long as the session should read as live — dropping it releases the flock.
/// The explicit `home` pins the marker inside the caller's [`HomeSandbox`], the
/// same tree `profile_dir` resolves under that sandbox.
pub(crate) fn arm_live_session(home: &Path, name: &str) -> std::fs::File {
    let sessions = home
        .join(".clauth")
        .join("profiles")
        .join(name)
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("test-pid")).expect("open pid");
    pid.lock().expect("lock pid");
    pid
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

/// Write a plain (diverged) `~/.claude/.credentials.json` carrying `creds`.
pub(crate) fn write_live_creds(creds: &crate::profile::ClaudeCredentials) {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&path, serde_json::to_vec(creds).expect("ser live")).expect("write live");
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
/// The cache the Alibaba arm writes for the captured Qwen Cloud monthly
/// payloads: one `30d` bar the provider marks as the account's inference quota,
/// the lite tier's 11500 allowance turning the ratio into `used / total`.
pub(crate) const QWEN_MONTHLY_CACHE: &str = r#"{"is_available":true,"rows":[],"bars":[{"label":"30d","pct":0.0095790974,"resets_at":"2026-11-11T16:00:00+00:00","used":1.101596201,"total":11500.0,"inference_quota":true}],"plan":"lite","best_effort":false}"#;

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

/// Parse a captured `third_party_cache.json`, re-anchor its bar stamps (see
/// [`write_captured_third_party_cache`]), and hand back the re-serialized
/// bytes — for a test whose route to disk is a raw file write rather than the
/// production cache writer.
pub(crate) fn reanchored_bars_cache_bytes(json: &str) -> Vec<u8> {
    let mut parsed: crate::providers::ThirdPartyStats =
        serde_json::from_str(json).expect("captured cache parses");
    let now = crate::usage::now_epoch_secs();
    for bar in &mut parsed.bars {
        // Each bar is stamped now + its own window length, read off the label
        // the provider itself wrote — not the captured stamp, whose offset
        // from another bar's can be internally inconsistent (one capture held
        // a 5h bar resetting days before its own 30d bar) and preserving it
        // would re-create lapsed bars as real time moves. An unparseable label
        // still gets a future stamp: a shape fixture is not a lapsed case.
        let span = crate::usage::window_duration_secs(&bar.label).unwrap_or(3600);
        bar.resets_at = Some(crate::usage::epoch_secs_to_iso(now + span));
    }
    serde_json::to_vec(&parsed).expect("re-serialized cache parses")
}

/// Parse a captured `third_party_cache.json` and write it at `name`'s
/// sandboxed path through the production cache writer — the same route the
/// fetch leg takes — so a consumer is driven by captured bytes, never a
/// hand-built [`crate::providers::ThirdPartyStats`] that mirrors the reader's
/// own guess. A captured bar's `resets_at` is an ABSOLUTE stamp, so real time
/// drifting past it turns a fixture meant to exercise the bars SHAPE into a
/// lapsed-window case the liveness gate legitimately drops: every parseable
/// bar stamp is re-stamped at now plus its own window length (see
/// [`reanchored_bars_cache_bytes`]), so a captured cache renders its shape
/// forever and lapsed behaviour is pinned only by the tests that mean it.
pub(crate) fn write_captured_third_party_cache(name: &str, json: &str) {
    let bytes = reanchored_bars_cache_bytes(json);
    let parsed: crate::providers::ThirdPartyStats =
        serde_json::from_slice(&bytes).expect("captured cache parses");
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
        harness: crate::harness::Harness::Claude,
        pid: 4242,
        claude_pid: None,
        launch_families: None,
        started_at: 1_700_000_000_000,
        cwd: None,
        isolated: false,
        follows_chain: true,
        intended_member: None,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        is_delegate: false,
        background: false,
        background_hold: false,
    }
}

/// Write `~/.clauth/status.json` carrying a daemon `gateway` slot in `state`,
/// so `daemon::gateway_slot(DaemonHealth::Fresh)` reads it.
pub(crate) fn write_daemon_gateway_slot(
    home: &HomeSandbox,
    state: crate::daemon::gateway::GatewayState,
) {
    let dir = home.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "gateway": {
            "state": state,
            "floor": "0.48.0",
            "restarts": 0,
        }
    });
    std::fs::write(dir.join("status.json"), body.to_string()).unwrap();
}

/// Adopt a `shunt.toml` holding `config_text` into `~/.clauth/gateway.toml`
/// with `disabled`, returning the record so a caller can delete its config for
/// the `no_config` verdict.
pub(crate) fn write_adopted_record(
    home: &HomeSandbox,
    disabled: bool,
    config_text: &str,
) -> crate::gateway::GatewayRecord {
    let config = home.home().join("etc").join("shunt.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, config_text).unwrap();
    let mut record = crate::gateway::GatewayRecord::new(config).unwrap();
    record.disabled = disabled;
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    record
}

/// The platform's own reason for reading `dir` as a file ("Is a directory" on
/// unix, "Access is denied." on Windows), for a fixture that plants a
/// directory where the code under test reads one and pins the OS cause.
pub(crate) fn directory_read_error(dir: &Path) -> String {
    std::fs::read_to_string(dir)
        .expect_err("a directory does not read")
        .to_string()
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
    // Mirror of `enforce_clauth_perms`: a codex home's contents are codex's
    // own (exec-bit helper binaries included), so the invariant covers the
    // home NODE and stops at its threshold.
    if is_dir && crate::runtime::is_codex_home_path(root) {
        return out;
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

/// Draw one frame through `draw`: the native caret it placed and the buffer.
pub(crate) fn draw_caret(
    term: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    draw: impl FnOnce(&mut ratatui::Frame<'_>),
) -> (ratatui::layout::Position, ratatui::buffer::Buffer) {
    term.draw(draw).expect("draw a frame");
    let caret = term.get_cursor_position().expect("read the caret");
    (caret, term.backend().buffer().clone())
}

/// The symbol a rendered buffer holds at `(x, y)`.
pub(crate) fn symbol_at(buf: &ratatui::buffer::Buffer, x: u16, y: u16) -> String {
    buf[(x, y)].symbol().to_string()
}

/// The column of the rightmost `│` on row `y`: the pane border a field's
/// value area ends two cells short of (its right padding between).
pub(crate) fn right_border(buf: &ratatui::buffer::Buffer, y: u16) -> u16 {
    (0..buf.area.width)
        .rev()
        .find(|&x| buf[(x, y)].symbol() == "│")
        .expect("the row has a right border")
}

/// Pins an edit frame against its resting frame when an edited row's block
/// changes height: every row above `field` matches whole, and the `count`
/// rows from `below` (the first row under the block at rest) match whole
/// `delta` rows lower in the edit frame (higher when `delta` is negative).
pub(crate) fn assert_rows_below_shift(
    resting: &[String],
    editing: &[String],
    field: usize,
    below: usize,
    delta: isize,
    count: usize,
    case: &str,
) {
    let frames = || {
        format!(
            "resting:\n{}\nediting:\n{}",
            resting.join("\n"),
            editing.join("\n")
        )
    };
    assert_eq!(
        editing[..field],
        resting[..field],
        "[{case}] the rows above the field hold still\n{}",
        frames()
    );
    for k in 0..count {
        let at = below + k;
        let moved = at
            .checked_add_signed(delta)
            .expect("the shifted row is on the frame");
        assert_eq!(
            editing[moved],
            resting[at],
            "[{case}] resting row {at} sits at row {moved} while editing\n{}",
            frames()
        );
    }
}

/// The cells of the first occurrence of `needle` in a rendered buffer, matched
/// cell by cell (one narrow glyph per cell), so a multibyte glyph earlier on
/// the row cannot shift the column the way a byte offset would.
pub(crate) fn needle_cells<'a>(
    buf: &'a ratatui::buffer::Buffer,
    needle: &str,
) -> Option<Vec<&'a ratatui::buffer::Cell>> {
    let want: Vec<String> = needle.chars().map(String::from).collect();
    let w = buf.area.width as usize;
    let h = buf.area.height as usize;
    (0..h).find_map(|y| {
        let row = &buf.content[y * w..(y + 1) * w];
        (0..=w.checked_sub(want.len())?).find_map(|x| {
            let cells = &row[x..x + want.len()];
            cells
                .iter()
                .zip(&want)
                .all(|(cell, ch)| cell.symbol() == ch)
                .then(|| cells.iter().collect())
        })
    })
}

/// `lines` drawn top-down into a `w`-wide buffer one row per line, so a span
/// builder's output can be read cell by cell like a full frame.
pub(crate) fn lines_buffer(
    lines: Vec<ratatui::text::Line<'static>>,
    w: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::widgets::Widget;
    let h = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h));
    ratatui::widgets::Paragraph::new(lines).render(buf.area, &mut buf);
    buf
}

/// Asserts how `phrase` renders where its `part` is a key or a typed command
/// in prose: every non-space
/// cell of `part` is `ACCENT`, bold for a key and not for a command, and no
/// other non-space cell of `phrase` is `ACCENT`.
pub(crate) fn assert_prose_part(
    buf: &ratatui::buffer::Buffer,
    phrase: &str,
    part: &str,
    is_key: bool,
) {
    let cells = needle_cells(buf, phrase)
        .unwrap_or_else(|| panic!("`{phrase}` missing:\n{}", buffer_rows(buf).join("\n")));
    let at = phrase
        .find(part)
        .map(|byte| phrase[..byte].chars().count())
        .unwrap_or_else(|| panic!("`{part}` is not inside `{phrase}`"));
    let len = part.chars().count();
    let accent = crate::tui::theme::accent_color();
    for (i, cell) in cells.iter().enumerate() {
        if cell.symbol() == " " {
            continue;
        }
        let bold = cell.modifier.contains(ratatui::style::Modifier::BOLD);
        if (at..at + len).contains(&i) {
            assert_eq!(cell.fg, accent, "`{part}` in `{phrase}`: cell {i} fg");
            assert_eq!(bold, is_key, "`{part}` in `{phrase}`: cell {i} bold");
        } else {
            assert_ne!(cell.fg, accent, "`{phrase}` outside `{part}`: cell {i} fg");
        }
    }
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

/// Poll the live-session registry until a row matches `stop` (or the deadline
/// passes), returning every observation in time order, duplicates across
/// passes kept — so a caller can assert a row was NEVER seen in a given state,
/// not just what the last pass held. The shared reader the mcp and `clauth
/// start` producer pins both call, so each producer's launcher choice is
/// observed through the registry it actually writes.
#[cfg(unix)]
pub(crate) fn live_session_rows_until(
    dir: &std::path::Path,
    stop: impl Fn(&crate::live_sessions::LiveSession) -> bool,
    timeout: std::time::Duration,
) -> Vec<crate::live_sessions::LiveSession> {
    let deadline = std::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let mut found = false;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let Ok(bytes) = std::fs::read(entry.path()) else {
                    continue;
                };
                let Ok(row) = serde_json::from_slice::<crate::live_sessions::LiveSession>(&bytes)
                else {
                    continue;
                };
                found |= stop(&row);
                seen.push(row);
            }
        }
        if found || std::time::Instant::now() > deadline {
            return seen;
        }
        // No pause between passes: a pause can skip a row state that lasts only
        // a moment, and the callers assert a state was never seen.
        std::thread::yield_now();
    }
}

/// Field 22 of `/proc/<pid>/stat` (the process start time in clock ticks since
/// boot), parsed after the last `)`, or `None` when the file is unreadable or
/// the field is missing. The one test-side parse, shared by the fixtures that
/// seed a record with the process's real start.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn proc_start_of(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    Some(rest.split_whitespace().nth(19)?.to_string())
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
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"2.1.220 (Claude Code)\"; exit 0;;\n  plugin) exit 0;;\nesac\necho $$ > \"$HOME/.slow-claude.pid\"\nsleep 5\nexit 0\n",
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

// ── Third-party stats fixtures ────────────────────────────────────────────────

/// The `ThirdPartyStats` shell every typed-provider fixture builds: available,
/// no rows, typed (never best-effort), carrying exactly the given bars — one
/// definition so the non-bar fields cannot drift between test modules.
pub(crate) fn stats_with_bars(
    bars: Vec<crate::providers::UsageBar>,
) -> crate::providers::ThirdPartyStats {
    crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars,
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

/// One unstamped percentage bar — the shape a provider's `5h`/`7d` windows
/// arrive as. [`bar_reset_in`] stamps one when a test needs liveness to hold.
pub(crate) fn bar(label: &str, pct: f64) -> crate::providers::UsageBar {
    crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: None,
        used: None,
        total: None,
        inference_quota: false,
    }
}

/// [`bar`] with a `resets_at` the given seconds into the future — a live
/// window, for the surfaces that judge liveness off the stamp.
pub(crate) fn bar_reset_in(
    label: &str,
    pct: f64,
    secs_in_future: i64,
) -> crate::providers::UsageBar {
    crate::providers::UsageBar {
        resets_at: Some(crate::usage::epoch_secs_to_iso(
            crate::usage::now_epoch_secs() + secs_in_future,
        )),
        ..bar(label, pct)
    }
}

/// The walk fixture reviewer A's r3 probe used: `act` and `ok` are OAuth
/// accounts (`act` exhausted at 99%, `ok` at 10% fresh or aged), `kl` is a
/// KEYLESS DeepSeek chain member — an `[env]` inference token (so the walk
/// keeps it eligible, and `load_profile` keeps its endpoint) but NO api key,
/// with a `third_party_cache.json` reading `kl_five`% — a display-only name
/// (third-party-classified, no api key), the exact shape whose cached reading
/// commit 2b's display superset lets reach the walk. The active's credentials
/// are linked so startup reconcile is Clean. Runs under the caller's
/// `HomeSandbox`.
pub(crate) fn keyless_walk_fixture(chain: &[&str], active: &str, ok_age_secs: u64, kl_five: f64) {
    use crate::profile::{AppState, ProfileName, save_app_state, save_profile};
    let oauth = |n: &str| {
        let mut p = blank_profile(&ProfileName::from(n));
        p.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: format!("acc-{n}"),
                refresh_token: Some(format!("rt-{n}")),
                expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
                scopes: None,
                subscription_type: None,
                ..crate::profile::OAuthToken::default_extra()
            }),
        });
        p
    };
    let act = oauth("act");
    let ok = oauth("ok");
    let mut kl = blank_profile(&ProfileName::from("kl"));
    kl.base_url = Some("https://api.deepseek.com/anthropic".into());
    kl.provider = Some(crate::providers::Provider::DeepSeek);
    kl.env
        .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "tok-kl".to_string());
    for p in [&act, &kl, &ok] {
        save_profile(p).expect("save profile");
    }
    save_app_state(&AppState {
        active_profile: Some(active.into()),
        profiles: vec!["act".into(), "kl".into(), "ok".into()],
        fallback_chain: chain.iter().map(|&n| n.into()).collect(),
        refresh_interval_ms: 90_000,
        ..AppState::default()
    })
    .expect("save state");
    let write_oauth_cache = |name: &str, five: f64, age_secs: u64| {
        let now = crate::usage::now_epoch_secs();
        let info = crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: five,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now + 7200)),
            }),
            fetched_at: Some(crate::usage::now_ms() - age_secs * 1000),
            ..crate::usage::UsageInfo::default()
        };
        crate::profile_cache::write_profile_cache(
            &ProfileName::from(name),
            crate::profile_cache::USAGE_CACHE_FILE,
            &info,
        );
        let path = crate::profile::profile_dir(&ProfileName::from(name))
            .unwrap()
            .join(crate::profile_cache::USAGE_CACHE_FILE);
        set_mtime(
            &path,
            std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs),
        );
    };
    write_oauth_cache("act", 99.0, 0);
    write_oauth_cache("ok", 10.0, ok_age_secs);
    crate::profile_cache::write_profile_cache(
        &ProfileName::from("kl"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats_with_bars(vec![bar_reset_in("5h", kl_five, 7200), bar("7d", 23.0)]),
    );
    if active != "kl" {
        crate::claude::force_link_profile_credentials(&ProfileName::from(active))
            .expect("link active");
    }
}

/// Resolve a `$ref` through the component schemas, following ref chains until a
/// concrete schema is reached.
pub(crate) fn schema_deref<'a>(
    schema: &'a RefOr<Schema>,
    components: &'a BTreeMap<String, RefOr<Schema>>,
) -> &'a Schema {
    match schema {
        RefOr::Ref(reference) => {
            let name = reference
                .ref_location
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected ref location {}", reference.ref_location));
            let component = components
                .get(name)
                .unwrap_or_else(|| panic!("unresolved component {name}"));
            schema_deref(component, components)
        }
        RefOr::T(schema) => schema,
    }
}

/// The JSON type name a serialized value carries, with an integral number read
/// as `integer` so a schema's `integer` admits only whole numbers.
fn value_json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(n) => {
            if n.as_f64().is_some_and(|f| f.fract() == 0.0) {
                "integer"
            } else {
                "number"
            }
        }
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn type_name(t: &Type) -> &'static str {
    match t {
        Type::Object => "object",
        Type::String => "string",
        Type::Integer => "integer",
        Type::Number => "number",
        Type::Boolean => "boolean",
        Type::Array => "array",
        Type::Null => "null",
    }
}

/// The JSON type names a schema's `type` allows; empty for a typeless schema.
fn allowed_type_names(schema_type: &SchemaType) -> Vec<&'static str> {
    match schema_type {
        SchemaType::Type(t) => vec![type_name(t)],
        SchemaType::Array(ts) => ts.iter().map(type_name).collect(),
        SchemaType::AnyValue => Vec::new(),
    }
}

/// Whether `vtype` is admitted by one of the allowed names: `number` admits any
/// numeric value, `integer` only integral ones.
fn type_matches(vtype: &str, allowed: &[&str]) -> bool {
    allowed.iter().any(|allowed| match *allowed {
        "number" => vtype == "number" || vtype == "integer",
        other => other == vtype,
    })
}

/// Walk `value` against `schema`, resolving `$ref`s through the component map,
/// and return the first mismatch named by its JSON path.
fn walk(
    value: &serde_json::Value,
    schema: &RefOr<Schema>,
    components: &BTreeMap<String, RefOr<Schema>>,
    path: &str,
) -> Result<(), String> {
    match schema_deref(schema, components) {
        Schema::Object(object) => object_agrees(value, object, components, path),
        Schema::Array(array) => array_agrees(value, array, components, path),
        Schema::OneOf(one_of) => {
            if one_of
                .items
                .iter()
                .any(|arm| walk(value, arm, components, path).is_ok())
            {
                Ok(())
            } else {
                Err(format!("{path}: the body {value} matches no oneOf arm"))
            }
        }
        Schema::AnyOf(any_of) => {
            if any_of
                .items
                .iter()
                .any(|arm| walk(value, arm, components, path).is_ok())
            {
                Ok(())
            } else {
                Err(format!("{path}: the body {value} matches no anyOf arm"))
            }
        }
        Schema::AllOf(all_of) => {
            for arm in &all_of.items {
                walk(value, arm, components, path)?;
            }
            Ok(())
        }
        _ => Err(format!(
            "{path}: the schema is a shape the walk cannot check"
        )),
    }
}

/// Check `value` against a schema rendered as an [`Object`], which utoipa uses
/// for every leaf type as well as for objects: the value's JSON type must be
/// one the schema's `type` names, and an object value keeps today's key checks
/// and recurses through `properties` and `additionalProperties`.
fn object_agrees(
    value: &serde_json::Value,
    object: &Object,
    components: &BTreeMap<String, RefOr<Schema>>,
    path: &str,
) -> Result<(), String> {
    let allowed = allowed_type_names(&object.schema_type);
    if allowed.is_empty() {
        return Err(format!(
            "{path}: the schema names no type, so the body cannot be checked"
        ));
    }
    let vtype = value_json_type(value);
    if !type_matches(vtype, &allowed) {
        return Err(format!(
            "{path}: the schema allows {} but the body is {vtype} ({value})",
            allowed.join("|")
        ));
    }
    if vtype == "object" {
        check_object_body(
            value.as_object().expect("json object"),
            object,
            components,
            path,
        )?;
    }
    Ok(())
}

fn check_object_body(
    body: &serde_json::Map<String, serde_json::Value>,
    object: &Object,
    components: &BTreeMap<String, RefOr<Schema>>,
    path: &str,
) -> Result<(), String> {
    for (property, property_schema) in &object.properties {
        let property_path = format!("{path}.{property}");
        if object.required.iter().any(|name| name == property) && !body.contains_key(property) {
            return Err(format!(
                "{property_path}: required schema property is absent from the body"
            ));
        }
        if let Some(property_value) = body.get(property) {
            walk(property_value, property_schema, components, &property_path)?;
        }
    }
    for (key, key_value) in body {
        if object.properties.contains_key(key) {
            continue;
        }
        let key_path = format!("{path}.{key}");
        match object.additional_properties.as_deref() {
            Some(AdditionalProperties::RefOr(schema)) => {
                walk(key_value, schema, components, &key_path)?;
            }
            Some(AdditionalProperties::FreeForm(_)) => {
                return Err(format!(
                    "{key_path}: additional properties are free-form, which the walk cannot check"
                ));
            }
            None => {
                return Err(format!("{key_path}: body key is absent from the schema"));
            }
        }
    }
    Ok(())
}

fn array_agrees(
    value: &serde_json::Value,
    array: &Array,
    components: &BTreeMap<String, RefOr<Schema>>,
    path: &str,
) -> Result<(), String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("{path}: the schema is an array but the body is {value}"))?;
    if let ArrayItems::RefOrSchema(item_schema) = &array.items {
        for (index, item) in items.iter().enumerate() {
            walk(item, item_schema, components, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

/// Walk `value` against `schema`, resolving `$ref`s through the document's
/// component schemas, and panic naming the JSON path of the first mismatch.
pub(crate) fn schema_agrees(
    value: &serde_json::Value,
    schema: &RefOr<Schema>,
    components: &Components,
) {
    if let Err(message) = walk(value, schema, &components.schemas, "$") {
        panic!("{message}");
    }
}

/// Derive the schema and component map from `T` and walk them against `value`:
/// the one entry every body's schema-truth test calls, so the walk stays
/// value-only and shared.
pub(crate) fn schema_agrees_with_type<T: ToSchema>(value: &serde_json::Value) {
    let mut schemas: Vec<(String, RefOr<Schema>)> = Vec::new();
    T::schemas(&mut schemas);
    schemas.push((T::name().into_owned(), T::schema()));
    let mut components = Components::new();
    components.schemas = schemas.into_iter().collect();
    schema_agrees(value, &T::schema(), &components);
}

// ── daemon-route test harness ─────────────────────────────────────────────────
//
// The request/context helpers every daemon-route test module shares: the
// control-device bearer constants, the profile seeders, the request builder,
// and the router call. Defined once here rather than copied per
// `tests/inline/*.rs` so a `Request` field or device-seeding change lands in
// one place. Every consumer is a `#![cfg(unix)]` test module, so the whole
// harness is unix-gated too: on the windows cross-lint it would otherwise read
// as dead code under `-D warnings`.

#[cfg(unix)]
mod route_harness {
    use std::net::SocketAddr;

    use crate::daemon::api::devices::{self, Tier};
    use crate::daemon::api::http::{Request, Response};
    use crate::daemon::api::routes::{ApiContext, handle};
    use crate::profile::{ClaudeCredentials, ConfigHandle, OAuthToken, Profile, save_profile};

    /// The bearer of the control device every context below pairs.
    pub(crate) const TOKEN: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    /// The device [`TOKEN`] authenticates as.
    pub(crate) const DEVICE: &str = "test";
    /// The bearer of a second device, which the tests that need one pair.
    pub(crate) const OTHER_TOKEN: &str =
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    pub(crate) fn creds(access: &str) -> ClaudeCredentials {
        ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..crate::profile::OAuthToken::default_extra()
            }),
        }
    }

    pub(crate) fn stored_profile(name: &str) -> Profile {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds(name));
        save_profile(&p).expect("save profile");
        p
    }

    /// A context over `config`, with [`TOKEN`] paired as the control device
    /// [`DEVICE`].
    pub(crate) fn ctx_with(config: ConfigHandle) -> std::sync::Arc<ApiContext> {
        seed_device(DEVICE, Tier::Control, TOKEN);
        let status_path = crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("status.json");
        ApiContext::for_tests(
            config,
            status_path,
            None,
            crate::daemon::api::panes::absent_probe(),
        )
    }

    /// A request as the HTTP layer would hand it to the router.
    pub(crate) fn req(method: &str, path: &str, bearer: Option<&str>, body: &str) -> Request {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            bearer: bearer.map(str::to_string),
            if_none_match: None,
            body: body.as_bytes().to_vec(),
            // Routing does not depend on this; the connection loop owns it.
            keep_alive: true,
            ws: Default::default(),
        }
    }

    pub(crate) fn peer() -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 7], 50_000))
    }

    /// The router as most of these tests drive it: one fixed peer, the answer
    /// alone. A test about which device the answer went to calls [`handle`].
    pub(crate) fn call(ctx: &ApiContext, req: &Request) -> Response {
        handle(ctx, req, peer()).response
    }

    pub(crate) fn seed_device(name: &str, tier: Tier, token: &str) {
        devices::seed_for_tests(name, tier, token).expect("seed a device");
    }

    pub(crate) fn body_json(resp: &Response) -> serde_json::Value {
        serde_json::from_slice(&resp.body).expect("response body is json")
    }

    /// Write a feed body to the context's `status.json`, for the tests that
    /// stage a stale feed before driving a republish.
    pub(crate) fn write_feed(ctx: &ApiContext, body: &str) {
        std::fs::write(&ctx.status_path, body).expect("write status.json");
    }
}

#[cfg(unix)]
pub(crate) use route_harness::*;

/// An executable shell shim: `#!/bin/sh` + `body`, mode 0755. The heal
/// fixtures resolve `herdr`/`git`/`claude` off `PATH`, so a test pins their
/// behavior by prepending the shim's dir (unix-only: shebang + exec bit).
#[cfg(unix)]
pub(crate) fn write_shim(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("shim written");
    let mut perms = std::fs::metadata(&path)
        .expect("shim metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("shim chmod");
    path
}

/// The POSIX-sh `record_start <pid> <file>` function a stub body splices in
/// to record a pid's start time in exactly the spelling
/// [`crate::daemon::gateway::process_start_time`] returns for the same pid,
/// so a [`SleepGuard`]'s drop-time comparison agrees with it on every unix. A
/// failed record leaves the file empty and never touches the stub's stderr (a
/// check reads that stderr to its bound), so a missing `/proc` or `ps` reds
/// nothing.
#[cfg(all(test, unix, target_os = "linux"))]
pub(crate) const RECORD_START_SH: &str = r#"record_start() { p=$1; f=$2; sed 's/.*) //' "/proc/$p/stat" 2>/dev/null | awk '{print $20}' > "$f" 2>/dev/null; }"#;

/// macOS and the BSDs: `ps -o lstart=` under `LC_ALL=C TZ=UTC0`, trimmed the
/// way [`crate::daemon::gateway::process_start_time`] trims its `lstart`.
#[cfg(all(test, unix, not(target_os = "linux")))]
pub(crate) const RECORD_START_SH: &str = r#"record_start() { p=$1; f=$2; v=$(LC_ALL=C TZ=UTC0 ps -o lstart= -p "$p" 2>/dev/null); v=${v#${v%%[![:space:]]*}}; v=${v%${v##*[![:space:]]}}; printf '%s' "$v" > "$f" 2>/dev/null; }"#;

/// A `sleep` a stub left running, its pid in `path` and its start time in the
/// sibling file named by appending `.start` to `path` (the stub records both
/// with [`RECORD_START_SH`]: a stub that `exec`s the sleep shares its pid and
/// `exec` keeps the start time, and one that backgrounds it writes the sleep's
/// own). Killed when the test unwinds while that pid is still live AND its
/// [`crate::daemon::gateway::process_start_time`] still equals the recorded
/// start, so a pid recycled to any other process — a `sleep` included — is
/// never signalled. Reads the files at drop, so a guard armed before the stub
/// ran still holds; best-effort, never asserts.
#[cfg(all(test, unix))]
pub(crate) struct SleepGuard(pub PathBuf);

#[cfg(all(test, unix))]
impl Drop for SleepGuard {
    fn drop(&mut self) {
        let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        else {
            return;
        };
        // A missing or empty record reads as "no verified pid to signal":
        // skip, never fail-loud — the guard is best-effort and a `Drop` that
        // asserts aborts the whole test binary.
        let recorded = match std::fs::read_to_string(start_file(&self.0)) {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => return,
        };
        if crate::daemon::gateway::process_start_time(pid).as_deref() != Some(recorded.as_str()) {
            return;
        }
        let _ = Command::new("kill")
            .args(["-s", "KILL", &pid.to_string()])
            .status();
    }
}

/// `path` with `.start` appended to its file name — never replacing an
/// extension — the [`SleepGuard`]'s recorded-start sibling.
#[cfg(all(test, unix))]
fn start_file(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".start");
    PathBuf::from(name)
}

/// A process the test owns outright, killed and reaped when the test unwinds,
/// so a red before its own kill leaks nothing. Never asserts (`Drop` runs
/// while a test unwinds; a panic there aborts the whole binary).
#[cfg(all(test, unix))]
pub(crate) struct Owned(pub std::process::Child);

#[cfg(all(test, unix))]
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A `git` shim answering `ls-remote --tags` with `$TAGS_OUTPUT` (the real
/// git's output shape) and recording every argv into `git.log`.
#[cfg(unix)]
pub(crate) fn git_shim(dir: &Path) -> PathBuf {
    write_shim(
        dir,
        "git",
        "echo \"$@\" >> \"$(dirname \"$0\")/git.log\"; if [ \"$1\" = \"ls-remote\" ] && [ \"$2\" = \"--tags\" ]; then printf '%s' \"$TAGS_OUTPUT\"; fi; exit 0",
    )
}

/// One lightweight release tag's `ls-remote --tags` line. (The annotated twin
/// — a tag-object line plus its peeled `^{}` pair — stays local to the herdr
/// tests, its only caller.)
#[cfg(unix)]
pub(crate) fn lightweight_tag(tag: &str, commit: &str) -> String {
    format!("{commit}\trefs/tags/{tag}\n")
}

/// The herdr heal shim: `plugin list --json` answers `$ANSWER_BEFORE` until an
/// install ran, then `$ANSWER_AFTER`; every other invocation logs into
/// `heal.log`, so a test pins exactly what the heal passed.
#[cfg(unix)]
pub(crate) fn stateful_heal_shim(dir: &Path) -> PathBuf {
    write_shim(
        dir,
        "herdr",
        "if [ \"$1\" = \"plugin\" ] && [ \"$2\" = \"list\" ]; then if [ -f \"$(dirname \"$0\")/installed\" ]; then echo \"$ANSWER_AFTER\"; else echo \"$ANSWER_BEFORE\"; fi; exit 0; fi; if [ \"$1\" = \"plugin\" ] && [ \"$2\" = \"install\" ]; then : > \"$(dirname \"$0\")/installed\"; fi; echo \"$@\" >> \"$(dirname \"$0\")/heal.log\"; exit 0",
    )
}

/// The herdr heal's own env: the shims ahead of `PATH`, the list answers, and
/// the `ls-remote --tags` body. `extra` pins test-specific vars on top.
#[cfg(unix)]
pub(crate) fn heal_env<'a>(
    home: &'a HomeSandbox,
    herdr_shim: &Path,
    before: &str,
    after: &str,
    tags: &str,
    extra: &[(&'static str, &std::ffi::OsStr)],
) -> EnvPin<'a> {
    let path = format!(
        "{}:{}",
        home.home().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut pins: Vec<(&'static str, Option<&std::ffi::OsStr>)> = vec![
        ("HERDR_BIN_PATH", Some(herdr_shim.as_os_str())),
        ("ANSWER_BEFORE", Some(std::ffi::OsStr::new(before))),
        ("ANSWER_AFTER", Some(std::ffi::OsStr::new(after))),
        ("TAGS_OUTPUT", Some(std::ffi::OsStr::new(tags))),
        ("PATH", Some(std::ffi::OsStr::new(&path))),
    ];
    for (key, value) in extra {
        pins.push((key, Some(*value)));
    }
    EnvPin::new(home, &pins)
}

/// Pin the pane env a herdr report test drives: `HERDR_PANE_ID` selects the
/// pane, `HERDR_BIN_PATH` (usually an echo shim) names the binary the report
/// spawns. Borrows the sandbox: the env is a process-global serialized by
/// `HOME_TEST_LOCK`, which the sandbox holds.
#[cfg(unix)]
pub(crate) fn herdr_pane_env<'a>(
    home: &'a HomeSandbox,
    pane: Option<&str>,
    bin: Option<&Path>,
) -> EnvPin<'a> {
    EnvPin::new(
        home,
        &[
            ("HERDR_PANE_ID", pane.map(std::ffi::OsStr::new)),
            ("HERDR_BIN_PATH", bin.map(Path::as_os_str)),
        ],
    )
}

/// Unset every `SHUNT_*` variable the gateway's client-token and bind reads
/// take from this process's env: shunt's default tokens variable, the bind
/// override and every env-layer `[server.auth]` key this process holds, in
/// whatever case it was exported, so a shell exporting one changes no result. Borrows the sandbox:
/// the env is a process-global serialized by `HOME_TEST_LOCK`, which the
/// sandbox holds.
pub(crate) fn no_shunt_env(home: &HomeSandbox) -> EnvPin<'_> {
    let auth_keys: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            crate::gateway::is_auth_env_key(name)
                || name.eq_ignore_ascii_case(crate::gateway::BIND_ENV)
        })
        .collect();
    let pins: Vec<(&str, Option<&std::ffi::OsStr>)> = std::iter::once("SHUNT_CLIENT_TOKENS")
        .chain(auth_keys.iter().map(String::as_str))
        .map(|key| (key, None))
        .collect();
    EnvPin::new(home, &pins)
}

/// Shim that appends its full argv to `report.log` beside itself and exits 0.
/// `$0` resolves to the shim itself, so the log needs no embedded path; a
/// report test spawns the shim as `HERDR_BIN_PATH` (or on `PATH`) and reads
/// the spawned argv back from the log.
#[cfg(unix)]
pub(crate) fn echo_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(dir, name, r#"echo "$@" >> "$(dirname "$0")/report.log""#)
}

/// Same log line, then exit 1: the caller must swallow the failing status.
#[cfg(unix)]
pub(crate) fn exit1_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(
        dir,
        name,
        "echo \"$@\" >> \"$(dirname \"$0\")/report.log\"\nexit 1",
    )
}

/// herdr's refusal of a resume report it cannot attach yet (no agent detected
/// in the pane), as its CLI prints it: the whole response on stderr, exit 1.
#[cfg(unix)]
const RESUME_NOT_ACCEPTED: &str = r#"echo '{"error":{"code":"resume_not_accepted","message":"resume_argv requires the reporter to hold the pane; report its state with pane.report_agent first"},"id":"cli:request"}' >&2"#;

/// Logs its argv, refuses the first call the way herdr does before it has
/// detected the pane's agent, and accepts every call after it.
#[cfg(unix)]
pub(crate) fn refuse_once_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(
        dir,
        name,
        &format!(
            "echo \"$@\" >> \"$(dirname \"$0\")/report.log\"\n\
             if [ ! -e \"$(dirname \"$0\")/refused\" ]; then\n\
             : > \"$(dirname \"$0\")/refused\"\n{RESUME_NOT_ACCEPTED}\nexit 1\nfi"
        ),
    )
}

/// Logs its argv and answers every call with herdr's error envelope for
/// `code` on stderr, exit 1.
#[cfg(unix)]
pub(crate) fn herdr_error_shim(dir: &Path, name: &str, code: &str) -> PathBuf {
    write_shim(
        dir,
        name,
        &format!(
            "echo \"$@\" >> \"$(dirname \"$0\")/report.log\"\n\
             echo '{{\"error\":{{\"code\":\"{code}\",\"message\":\"refused\"}},\"id\":\"cli:request\"}}' >&2\n\
             exit 1"
        ),
    )
}

/// Logs its argv and refuses every call the way herdr does before it has
/// detected the pane's agent.
#[cfg(unix)]
pub(crate) fn refuse_always_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(
        dir,
        name,
        &format!("echo \"$@\" >> \"$(dirname \"$0\")/report.log\"\n{RESUME_NOT_ACCEPTED}\nexit 1"),
    )
}

/// Logs its argv, then sleeps past a bounded caller's kill deadline. `exec` so
/// the kill takes the whole process.
#[cfg(unix)]
pub(crate) fn hang_shim(dir: &Path, name: &str) -> PathBuf {
    write_shim(
        dir,
        name,
        "echo \"$@\" >> \"$(dirname \"$0\")/report.log\"; exec sleep 30",
    )
}

/// Every line the shims in `dir` recorded so far (empty when none).
#[cfg(unix)]
pub(crate) fn report_lines(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("report.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Poll `report_lines` until at least `want` lines exist or `timeout` elapses.
#[cfg(unix)]
pub(crate) fn wait_for_lines(dir: &Path, want: usize, timeout: std::time::Duration) -> Vec<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let lines = report_lines(dir);
        if lines.len() >= want {
            return lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "shim log stalled at {} lines (want {want})",
            lines.len()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A Claude Code background job record, in the key set and order a live
/// `claude --bg` wrote on 2.1.303 (`~/.claude/jobs/<id>/state.json`, captured
/// 2026-10-19), naming `config_dir` as the job's `providerEnv.CLAUDE_CONFIG_DIR`.
/// Returns the job dir, which a test removes to pose `claude rm`.
pub(crate) fn write_claude_job(home: &Path, id: &str, config_dir: &Path) -> PathBuf {
    write_claude_job_in(home, id, config_dir, "working", None)
}

/// [`write_claude_job`] with the record's `state` and `firstTerminalAt` set.
/// Shapes captured on 2.1.304 (2026-10-20): `claude stop` writes `"stopped"`
/// with `firstTerminalAt` stamped, and `claude attach` on that stopped job
/// revives it as `"running"` with the stamp kept.
pub(crate) fn write_claude_job_in(
    home: &Path,
    id: &str,
    config_dir: &Path,
    state: &str,
    first_terminal_at: Option<&str>,
) -> PathBuf {
    let dir = home.join(".claude").join("jobs").join(id);
    std::fs::create_dir_all(dir.join("tmp")).expect("mkdir job dir");
    let record = serde_json::json!({
        "state": state,
        "detail": "(idle — send a prompt to start)",
        "tempo": "blocked",
        "needs": "send a prompt to start",
        "output": null,
        "children": null,
        "linkScanOffset": 0,
        "template": "bg",
        "respawnFlags": [],
        "providerEnv": { "CLAUDE_CONFIG_DIR": config_dir.to_string_lossy() },
        "intent": "",
        "sessionId": format!("{id}-0a6e-4b11-883e-fca3a218ae52"),
        "resumeSessionId": format!("{id}-0a6e-4b11-883e-fca3a218ae52"),
        "daemonShort": id,
        "cwd": "/home/dev/proj",
        "createdAt": "2026-10-19T05:25:39.445Z",
        "updatedAt": "2026-10-19T05:25:39.445Z",
        "firstTerminalAt": first_terminal_at,
        "backend": "daemon",
    });
    std::fs::write(
        dir.join("state.json"),
        serde_json::to_vec(&record).expect("encode job"),
    )
    .expect("write job");
    dir
}

/// [`write_claude_job`] with the record's `respawnFlags` set to `flags`. A
/// clauth-launched job carries `["--system-prompt-file", <path>, "--settings",
/// <spawn-settings file>]` there (captured 2026-10-20, 2.1.304); a test passes
/// any JSON value to pose a retyped field.
pub(crate) fn write_claude_job_flags(
    home: &Path,
    id: &str,
    config_dir: &Path,
    flags: serde_json::Value,
) -> PathBuf {
    let dir = write_claude_job(home, id, config_dir);
    let path = dir.join("state.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read job")).expect("parse job");
    record["respawnFlags"] = flags;
    std::fs::write(&path, serde_json::to_vec(&record).expect("encode job")).expect("write job");
    dir
}

/// The `respawnFlags` a `clauth start <p> -- --bg` job carries for `settings`,
/// in the captured order.
pub(crate) fn respawn_flags_naming(settings: &Path) -> serde_json::Value {
    serde_json::json!([
        "--system-prompt-file",
        "/home/dev/.claude/system-prompt/cc-sys.md",
        "--settings",
        settings.to_string_lossy(),
    ])
}

/// The `--seq` value of one recorded report line (the token after the flag).
#[cfg(unix)]
pub(crate) fn seq_of(line: &str) -> u64 {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--seq" {
            return tokens
                .next()
                .expect("seq value")
                .parse()
                .expect("seq is numeric");
        }
    }
    panic!("no --seq in report line: {line}");
}
