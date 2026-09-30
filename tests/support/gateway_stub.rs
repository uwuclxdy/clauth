//! A stub `shunt` for the gateway supervisor's tests, shared by the inline
//! supervisor tests (`tests/inline/daemon_gateway.rs`) and the real-daemon
//! test (`tests/gateway_daemon.rs`).
//!
//! The stub is a `/bin/sh` script: it records each run (the path it ran as,
//! argv, cwd, the `SHUNT_*`, `CODEX_AUTH_FILE` and `CLAUDE_CREDENTIALS` env
//! it saw, the env file's test variable) and
//! each SIGTERM it gets, writes one line to stdout and one to stderr, and then
//! holds a named pipe open for as long as it lives. [`HealthServer`] serves
//! `/health` on the gateway's port exactly while a writer holds that pipe, so
//! the port answers while a stub process is alive and falls silent the moment
//! it dies, which a shell script cannot do by listening itself. Unix only: a
//! shell script and a named pipe.
#![allow(
    dead_code,
    reason = "the two including crates each use their own subset of these helpers"
)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A stub's lifetime cap, so a stub a failing test left behind ends on its
/// own.
const STUB_LIFETIME_SECS: u32 = 120;

/// A loopback port nothing listens on now.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind an ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Poll `ready` every 10 ms until it holds, failing the test naming `what`
/// once `within` passes: a hang detector, not a race bound.
pub fn wait_until(what: &str, within: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "gave up after {within:?} waiting until {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Write the stub `shunt` into `dir` and create the pipe it holds. Every
/// SIGTERM a run gets is recorded in `dir/signals`; the run then dies of it.
/// Files in `dir` steer a run: `ignore-term` makes it record SIGTERM and live
/// on, `no-health` makes it skip the pipe, so nothing ever answers for it.
///
/// The trap is set before the run's record is written, so a test that has
/// read the record knows the trap is in place. The script stays the process
/// clauth spawned (no `exec`), since an `exec` drops a trap; its `sleep`s run
/// with the pipe closed and are waited for in the background, so a signal is
/// handled at once and the pipe closes the moment the script dies.
pub fn write_stub(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).expect("stub dir");
    let fifo = dir.join("fifo");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", fifo.display());
    let path = dir.join("shunt");
    let script = format!(
        "#!/bin/sh\n\
         d='{dir}'\n\
         if [ -e \"$d/ignore-term\" ]; then\n\
         \x20 trap 'echo \"term $$\" >> \"$d/signals\"' TERM\n\
         else\n\
         \x20 trap 'echo \"term $$\" >> \"$d/signals\"; trap - TERM; kill -s TERM $$' TERM\n\
         fi\n\
         {{\n\
         \x20 echo \"pid $$\"\n\
         \x20 echo \"bin $0\"\n\
         \x20 for a in \"$@\"; do echo \"arg $a\"; done\n\
         \x20 echo \"cwd $(pwd -P)\"\n\
         \x20 env | grep -e '^SHUNT_' -e '^CODEX_AUTH_FILE=' -e '^CLAUDE_CREDENTIALS=' | LC_ALL=C sort | sed 's/^/env /'\n\
         \x20 echo \"secret ${{GATEWAY_TEST_SECRET-}}\"\n\
         \x20 echo end\n\
         }} >> \"$d/calls\"\n\
         echo \"gateway stub stdout $$\"\n\
         echo \"gateway stub stderr $$\" >&2\n\
         if [ ! -e \"$d/no-health\" ]; then exec 3>\"$d/fifo\"; fi\n\
         i=0\n\
         while [ \"$i\" -lt {STUB_LIFETIME_SECS} ]; do\n\
         \x20 sleep 1 3>&- &\n\
         \x20 wait $!\n\
         \x20 i=$((i + 1))\n\
         done\n",
        dir = dir.display(),
    );
    std::fs::write(&path, script).expect("write the stub");
    let chmod = Command::new("chmod")
        .args(["755"])
        .arg(&path)
        .status()
        .expect("run chmod");
    assert!(chmod.success(), "chmod the stub");
    path
}

/// Write a stub `clauth-<service>-proxy` into `dir` and create the pipe it
/// holds. `manifest` records its pid in `dir/manifests`, sleeps 3 s when the
/// `slow-manifest` marker is present, and prints the manifest JSON, its
/// `version`, `contract` and `drain_secs` steered by the `version`, `contract`
/// and `drain` marker files (defaults `1.2.0`, `1.0`, `0`); `serve` records
/// its argv, cwd and the three `CLAUTH_PROXY_*` variables, answers `/health`
/// through the named pipe, and records each SIGTERM, waiting `drain` seconds
/// before it dies. Files in `dir` steer a run: `ignore-term` (record TERM and
/// live on), `no-health` (skip the pipe).
pub fn write_proxy_stub(dir: &Path, service: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("stub dir");
    let fifo = dir.join("fifo");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", fifo.display());
    let path = dir.join(format!("clauth-{service}-proxy"));
    let script = format!(
        "#!/bin/sh\n\
         d='{dir}'\n\
         if [ \"$1\" = \"manifest\" ]; then\n\
         \x20 echo \"$$\" >> \"$d/manifests\"\n\
         \x20 if [ -e \"$d/slow-manifest\" ]; then sleep 3; fi\n\
         \x20 ver=\"$(cat \"$d/version\" 2>/dev/null || echo 1.2.0)\"\n\
         \x20 con=\"$(cat \"$d/contract\" 2>/dev/null || echo 1.0)\"\n\
         \x20 drain=\"$(cat \"$d/drain\" 2>/dev/null || echo 0)\"\n\
         \x20 printf '{{\"service\":\"{service}\",\"display_name\":\"\",\"version\":\"%s\",\"contract\":\"%s\",\"capabilities\":[],\"drain_secs\":%s}}\\n' \"$ver\" \"$con\" \"$drain\"\n\
         \x20 exit 0\n\
         fi\n\
         if [ ! \"$1\" = \"serve\" ]; then exit 2; fi\n\
         if [ -e \"$d/ignore-term\" ]; then\n\
         \x20 trap 'echo \"term $$\" >> \"$d/signals\"' TERM\n\
         else\n\
         \x20 trap 'echo \"term $$\" >> \"$d/signals\"; trap - TERM; drain=\"$(cat \"$d/drain\" 2>/dev/null || echo 0)\"; sleep \"$drain\"; kill -s TERM $$' TERM\n\
         fi\n\
         {{\n\
         \x20 echo \"pid $$\"\n\
         \x20 echo \"bin $0\"\n\
         \x20 for a in \"$@\"; do echo \"arg $a\"; done\n\
         \x20 echo \"cwd $(pwd -P)\"\n\
         \x20 env | grep -e '^CLAUTH_PROXY_' | LC_ALL=C sort | sed 's/^/env /'\n\
         \x20 echo end\n\
         }} >> \"$d/calls\"\n\
         echo \"proxy stub stdout $$\"\n\
         echo \"proxy stub stderr $$\" >&2\n\
         if [ ! -e \"$d/no-health\" ]; then exec 3>\"$d/fifo\"; fi\n\
         i=0\n\
         while [ \"$i\" -lt {STUB_LIFETIME_SECS} ]; do\n\
         \x20 sleep 1 3>&- &\n\
         \x20 wait $!\n\
         \x20 i=$((i + 1))\n\
         done\n",
        dir = dir.display(),
        service = service,
    );
    std::fs::write(&path, script).expect("write the proxy stub");
    let chmod = Command::new("chmod")
        .args(["755"])
        .arg(&path)
        .status()
        .expect("run chmod");
    assert!(chmod.success(), "chmod the proxy stub");
    path
}

/// One recorded run of the stub.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub pid: u32,
    /// The path the run was started as.
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// `KEY=VALUE` for every `SHUNT_*` variable, `CODEX_AUTH_FILE` and
    /// `CLAUDE_CREDENTIALS` the run saw, sorted.
    pub env: Vec<String>,
    /// `GATEWAY_TEST_SECRET` as the run saw it.
    pub secret: String,
}

impl Invocation {
    /// The nine credential-location variables alone, as `KEY=VALUE`.
    pub fn store_env(&self) -> Vec<String> {
        const STORES: [&str; 9] = [
            "SHUNT_CLAUDE_ACCOUNTS_DIR",
            "SHUNT_CODEX_ACCOUNTS_DIR",
            "SHUNT_KIMI_ACCOUNTS_DIR",
            "SHUNT_ANTIGRAVITY_ACCOUNTS_DIR",
            "SHUNT_XAI_AUTH_FILE",
            "SHUNT_CURSOR_AUTH_FILE",
            "SHUNT_ANTIGRAVITY_AUTH_FILE",
            "CODEX_AUTH_FILE",
            "CLAUDE_CREDENTIALS",
        ];
        self.env
            .iter()
            .filter(|pair| {
                pair.split_once('=')
                    .is_some_and(|(key, _)| STORES.contains(&key))
            })
            .cloned()
            .collect()
    }
}

/// Every run the stub in `dir` recorded, oldest first; a run still writing
/// its record is not listed.
pub fn invocations(dir: &Path) -> Vec<Invocation> {
    let text = std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
    let mut runs = Vec::new();
    let mut run = Invocation::default();
    for line in text.lines() {
        let (tag, value) = line.split_once(' ').unwrap_or((line, ""));
        match tag {
            "pid" => run.pid = value.parse().expect("a recorded pid"),
            "bin" => run.bin = PathBuf::from(value),
            "arg" => run.args.push(value.to_string()),
            "cwd" => run.cwd = PathBuf::from(value),
            "env" => run.env.push(value.to_string()),
            "secret" => run.secret = value.to_string(),
            "end" => runs.push(std::mem::take(&mut run)),
            other => panic!("unexpected stub record line {other:?}"),
        }
    }
    runs
}

/// Every pid the stub in `dir` recorded, best-effort: a torn or unexpected
/// line is skipped, never a panic (a fixture `Drop` calls it).
pub fn recorded_pids(dir: &Path) -> Vec<u32> {
    let text = std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
    text.lines()
        .filter_map(|line| line.strip_prefix("pid ").and_then(|pid| pid.parse().ok()))
        .collect()
}

/// The pid of every run in `dir` that got a SIGTERM, once per signal, oldest
/// first.
pub fn terms(dir: &Path) -> Vec<u32> {
    let text = std::fs::read_to_string(dir.join("signals")).unwrap_or_default();
    text.lines()
        .map(|line| {
            let pid = line.strip_prefix("term ").expect("a recorded SIGTERM");
            pid.parse().expect("a recorded pid")
        })
        .collect()
}

/// `kill -s <signal> <pid>`, for a process the test itself owns.
pub fn signal(pid: u32, name: &str) {
    let sent = Command::new("kill")
        .args(["-s", name, &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success(), "kill -s {name} {pid}");
}

/// `kill -s <signal> <pid>` best-effort, for a fixture's teardown: a pid
/// already gone is ignored, never an assert.
pub fn signal_best_effort(pid: u32, name: &str) {
    let _ = Command::new("kill")
        .args(["-s", name, &pid.to_string()])
        .status();
}

/// Whether `pid` still exists (`kill -s 0`), for a process the test owns.
pub fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-s", "0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Best-effort SIGKILL for a stub pid the test started, but only while its
/// command line still names `stub` (a recycled pid no longer does, and must
/// not be signalled); a pid that is gone or no longer the stub is skipped
/// silently.
pub fn kill_best_effort(pid: u32, stub: &Path) {
    if !names_stub(pid, stub) {
        return;
    }
    let _ = Command::new("kill")
        .args(["-s", "KILL", &pid.to_string()])
        .status();
}

/// Whether `pid`'s command line still names `stub`, the stub script this test
/// wrote, as one whole argv word (the script's path after `/bin/sh`): the pid
/// may have been recycled, so it is only a stub while it names the test's own
/// `<dir>/shunt`, never a line merely holding that path inside another word.
pub fn names_stub(pid: u32, stub: &Path) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .any(|word| Path::new(word) == stub)
}

/// An HTTP answerer on a loopback port: every request gets `status` and
/// `body`, until it drops.
pub struct HttpAnswer {
    stop: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl HttpAnswer {
    /// Bind `port` (retrying while a listener this process just dropped
    /// releases it) and answer there.
    pub fn bind(port: u16, status: u16, body: &str) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => break listener,
                Err(e) => {
                    assert!(Instant::now() < deadline, "bind 127.0.0.1:{port}: {e}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            if status == 200 { "OK" } else { "Not Found" },
            body.len()
        );
        let thread = std::thread::spawn({
            let (stop, hits) = (Arc::clone(&stop), Arc::clone(&hits));
            move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            answer(stream, response.as_bytes());
                            hits.fetch_add(1, Ordering::AcqRel);
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            }
        });
        Self {
            stop,
            hits,
            thread: Some(thread),
        }
    }

    /// Requests answered so far.
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Acquire)
    }
}

impl Drop for HttpAnswer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read the request head (a GET carries no body), then write `response`.
fn answer(mut stream: TcpStream, response: &[u8]) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    let _ = stream.write_all(response);
    let _ = stream.flush();
}

/// A listener that accepts every connection and never answers, standing in
/// for a wedged answerer whose connect succeeds and whose response never
/// comes: a probe of it reads `NoAnswer`, never `Silent`.
pub struct HoldListener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HoldListener {
    /// Bind `port` (retrying while a listener this process just dropped
    /// releases it) and hold every connection open until it drops.
    pub fn bind(port: u16) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => break listener,
                Err(e) => {
                    assert!(Instant::now() < deadline, "bind 127.0.0.1:{port}: {e}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut held = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => held.push(stream),
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for HoldListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `/health` on a gateway port, answered with `version` while a stub process
/// holds the pipe in its dir and silent otherwise.
pub struct HealthServer {
    fifo: PathBuf,
    stop: Arc<AtomicBool>,
    serving: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HealthServer {
    pub fn start(stub_dir: &Path, port: u16, version: &str) -> Self {
        let body = format!(r#"{{"status":"ok","version":"{version}"}}"#);
        Self::start_body(stub_dir, port, body)
    }

    /// `/health` answered with a proxy's body while the stub holds the pipe.
    pub fn start_proxy(
        stub_dir: &Path,
        port: u16,
        service: &str,
        version: &str,
        contract: &str,
    ) -> Self {
        let body = format!(
            r#"{{"status":"ok","service":"{service}","version":"{version}","contract":"{contract}"}}"#
        );
        Self::start_body(stub_dir, port, body)
    }

    /// A proxy `/health` lacking the `service` field, for the refusal.
    pub fn start_proxy_without_service(
        stub_dir: &Path,
        port: u16,
        version: &str,
        contract: &str,
    ) -> Self {
        let body = format!(r#"{{"status":"ok","version":"{version}","contract":"{contract}"}}"#);
        Self::start_body(stub_dir, port, body)
    }

    fn start_body(stub_dir: &Path, port: u16, body: String) -> Self {
        let fifo = stub_dir.join("fifo");
        let stop = Arc::new(AtomicBool::new(false));
        let serving = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let (fifo, stop, serving) = (fifo.clone(), Arc::clone(&stop), Arc::clone(&serving));
            move || {
                while !stop.load(Ordering::Acquire) {
                    // Blocks until a stub (or the teardown's wake-up) opens
                    // the pipe for writing.
                    let Ok(mut pipe) = File::open(&fifo) else {
                        return;
                    };
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let answering = HttpAnswer::bind(port, 200, &body);
                    serving.store(true, Ordering::Release);
                    // EOF once every writer is gone: the stub died.
                    let mut sink = [0u8; 64];
                    while matches!(pipe.read(&mut sink), Ok(n) if n > 0) {}
                    drop(answering);
                    serving.store(false, Ordering::Release);
                }
            }
        });
        Self {
            fifo,
            stop,
            serving,
            thread: Some(thread),
        }
    }

    /// Whether the port answers now.
    pub fn serving(&self) -> bool {
        self.serving.load(Ordering::Acquire)
    }

    /// Wait until the port answers (`true`) or has fallen silent (`false`).
    pub fn wait_serving(&self, serving: bool) {
        wait_until(
            if serving {
                "the stub's port answers"
            } else {
                "the stub's port falls silent"
            },
            Duration::from_secs(10),
            || self.serving() == serving,
        );
    }
}

impl Drop for HealthServer {
    /// Every stub must already be gone: a live writer keeps the reader
    /// blocked, so this gives up joining after 30 s rather than hang.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                eprintln!("gateway stub: a stub still holds {}", self.fifo.display());
                return;
            }
            // Wakes a reader parked in `open`; refused (ENXIO) while none is.
            let _ = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.fifo);
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = thread.join();
    }
}
