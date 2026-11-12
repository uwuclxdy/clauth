//! The real TUI on a real terminal: a closed terminal (its pty master gone, as
//! when a terminal window or a herdr pane closes) and a SIGTERM each end it
//! through its clean shutdown, exiting `128 + signal`, also on a terminal that
//! stopped reading its output; and a terminal whose output dies under a live
//! frame loop ends it with an error exit, never a crash. Only a real pty shows
//! these: on a hung-up terminal crossterm's event read never returns, and a
//! terminal that stopped reading blocks a draw's write, so no in-process seam
//! replays either.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the TUI may take to exit after its terminal closes or a signal
/// lands: a hang detector, far above the shutdown's own waits.
const EXIT_BOUND: Duration = Duration::from_secs(30);

/// How long the TUI gets to draw its first frame.
const PAINT_BOUND: Duration = Duration::from_secs(30);

/// Bytes past the alternate-screen switch that prove a frame was drawn: the
/// switch comes before the frame loop starts, a drawn frame after it.
const FRAME_BYTES: usize = 500;

const ALT_SCREEN: &[u8] = b"\x1b[?1049h";

/// How long the TUI keeps running after its first frame before the test acts:
/// the frame loop reaches its first event read within one 80 ms frame of that
/// draw, and a terminal closes in practice while the loop waits there, inside
/// crossterm's read. A close before that first read leaves the loop another
/// way, so it would not replay the hang this file guards.
const SETTLE: Duration = Duration::from_secs(2);

/// How long a TUI must still be running after its terminal closed to count as
/// wedged in its event read: just under the TUI's one-second stop grace, so an
/// exit before it is the frame loop leaving on its own (its stop check, or a
/// draw that failed on the closed terminal), never the watcher's fallback. A
/// loop that left on its own and then spends longer than this in its
/// shutdown still counts as wedged; nothing outside the process tells the two
/// apart.
const WEDGED_AFTER: Duration = Duration::from_millis(900);

/// Closed-terminal attempts before a test gives up on reaching the wedged read.
const ATTEMPTS: usize = 3;

/// Held from `openpty` until the child is spawned: the pty's fds are not
/// close-on-exec until `cloexec` marks them, and a sibling test's child forked
/// in that window would keep the master open past this test's close.
static PTY_OPEN: Mutex<()> = Mutex::new(());

/// A `clauth` TUI running as the session leader of a fresh pty, killed with
/// its terminals if the test ends first.
struct Tui {
    child: Child,
    /// The master of the pty the TUI reads keys from, its controlling terminal.
    input: Option<File>,
    /// The master of a second pty holding the TUI's stdout and stderr, when
    /// they do not share the input pty.
    output: Option<File>,
    layout: Output,
}

/// What the TUI inherits for SIGHUP: the default, or ignored as `nohup` leaves it.
#[derive(Clone, Copy)]
enum Hangup {
    Default,
    Ignored,
}

/// Where the TUI's stdout and stderr go: the input pty, as in a real terminal,
/// or a pty of their own, so the output can die while the input stays live.
#[derive(Clone, Copy)]
enum Output {
    SameTerminal,
    OwnTerminal,
}

/// A fresh pty pair, both ends close-on-exec. The caller holds [`PTY_OPEN`].
fn open_pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` writes two open fds into the out-params; it gets no
    // name buffer, the default termios and a winsize that outlives the call
    // (a raw pointer: linux takes it `*const`, macOS `*mut`).
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: `openpty` just returned both fds open, and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    cloexec(&master);
    cloexec(&slave);
    (master, slave)
}

impl Tui {
    fn spawn(home: &Path, hangup: Hangup) -> Self {
        Self::spawn_with(home, hangup, Output::SameTerminal)
    }

    fn spawn_with(home: &Path, hangup: Hangup, output: Output) -> Self {
        let guard = PTY_OPEN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (master, slave) = open_pty();
        let (output_master, output_slave) = match output {
            Output::SameTerminal => (None, slave.try_clone().unwrap()),
            Output::OwnTerminal => {
                let (master, slave) = open_pty();
                (Some(master), slave)
            }
        };

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clauth"));
        cmd.env_clear()
            .env("HOME", home)
            .env("XDG_DATA_HOME", home.join("xdg"))
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("CLAUTH_NO_UPDATE", "1")
            .env("LC_ALL", "C")
            .stdin(Stdio::from(slave))
            .stdout(Stdio::from(output_slave.try_clone().unwrap()))
            .stderr(Stdio::from(output_slave));
        // SAFETY: the hook runs in the forked child before exec, after std put
        // the ptys' slaves on fds 0-2, and calls only async-signal-safe functions.
        unsafe {
            cmd.pre_exec(move || {
                // A new session whose controlling terminal is the pty, so the
                // master's close hangs up this process as a terminal close does.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if matches!(hangup, Hangup::Ignored)
                    && libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn the clauth TUI");
        drop(guard);
        Self {
            child,
            input: Some(File::from(master)),
            output: output_master.map(File::from),
            layout: output,
        }
    }

    /// Read the terminal until the TUI has drawn its first frame, then keep it
    /// drained for [`SETTLE`], red if it exits meanwhile.
    fn wait_for_frame(&mut self) {
        self.wait_for_first_frame();
        self.keep_drained(SETTLE);
    }

    fn wait_for_first_frame(&mut self) {
        let deadline = Instant::now() + PAINT_BOUND;
        let mut seen = Vec::new();
        loop {
            self.drain(&mut seen);
            let drawn = seen
                .windows(ALT_SCREEN.len())
                .position(|w| w == ALT_SCREEN)
                .is_some_and(|at| seen.len() - at >= FRAME_BYTES);
            if drawn {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "the TUI exited before its first frame ({status}): {}",
                    String::from_utf8_lossy(&seen)
                );
            }
            assert!(
                Instant::now() < deadline,
                "the TUI drew no frame within {PAINT_BOUND:?} ({} bytes read)",
                seen.len()
            );
        }
    }

    /// Close the pty master, as a terminal window or herdr pane closing does.
    fn close_terminal(&mut self) {
        self.input = None;
        self.output = None;
    }

    /// Close only the output pty's master: the frame loop's next draw fails
    /// while its key reads stay live, so the loop leaves on its own.
    fn close_output(&mut self) {
        self.output = None;
    }

    fn signal(&self, name: &str) {
        // `kill` is a child too: hold the pty window shut while it forks.
        let _guard = PTY_OPEN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sent = Command::new("kill")
            .args([&format!("-{name}"), &self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success(), "kill -{name} failed: {sent}");
    }

    /// Reap the TUI, red if it is still running `EXIT_BOUND` after `after`.
    fn wait_exit(&mut self, after: &str) -> ExitStatus {
        self.wait_exit_reading(after).0
    }

    /// Stop reading the screen and resize the terminal until the TUI's frame
    /// loop has sat blocked in a write for a second: each resize repaints the
    /// whole frame, so the unread pty fills and the draw's write blocks.
    #[cfg(target_os = "linux")]
    fn stall_screen(&mut self) {
        let deadline = Instant::now() + PAINT_BOUND;
        let mut blocked_since: Option<Instant> = None;
        let mut wide = false;
        loop {
            if self.main_thread_writing() {
                let since = *blocked_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(1) {
                    return;
                }
            } else {
                blocked_since = None;
                wide = !wide;
                self.resize(80, if wide { 250 } else { 249 });
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("the TUI exited while its screen filled ({status})");
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never blocked writing to its unread screen within {PAINT_BOUND:?}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The syscall the TUI's main thread, its frame loop, sits in, if any.
    #[cfg(target_os = "linux")]
    fn main_thread_syscall(&self) -> Option<libc::c_long> {
        let pid = self.child.id();
        let path = format!("/proc/{pid}/task/{pid}/syscall");
        let call = match std::fs::read_to_string(&path) {
            Ok(call) => call,
            // The file needs ptrace-attach rights (yama `ptrace_scope`).
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                panic!("{path} is unreadable here, so the frame loop's syscall cannot be seen: {e}")
            }
            Err(_) => return None,
        };
        call.split_whitespace().next()?.parse().ok()
    }

    /// Whether the frame loop sits in `write`/`writev`.
    #[cfg(target_os = "linux")]
    fn main_thread_writing(&self) -> bool {
        self.main_thread_syscall()
            .is_some_and(|number| number == libc::SYS_write || number == libc::SYS_writev)
    }

    /// Whether the frame loop sits in its event poll (mio's epoll wait).
    #[cfg(target_os = "linux")]
    fn main_thread_polling(&self) -> bool {
        self.main_thread_syscall().is_some_and(|number| {
            #[cfg(target_arch = "x86_64")]
            if number == libc::SYS_epoll_wait {
                return true;
            }
            number == libc::SYS_epoll_pwait
        })
    }

    /// Type `keys` into the input terminal.
    #[cfg(target_os = "linux")]
    fn send_keys(&self, keys: &[u8]) {
        use std::io::Write as _;
        let mut master = self.input.as_ref().expect("the input terminal is open");
        master.write_all(keys).expect("type into the terminal");
    }

    /// Keep the screen drained for `span`, red if the TUI exits meanwhile.
    fn keep_drained(&mut self, span: Duration) {
        let until = Instant::now() + span;
        let mut seen = Vec::new();
        while Instant::now() < until {
            self.drain(&mut seen);
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("the TUI exited on its own ({status})");
            }
        }
    }

    /// Stop the terminal's output (TCOOFF on the pty slave, so every write
    /// blocks however much room the pty has) while the frame loop sits in its
    /// event poll, held there by SIGSTOP until `stimulus` is queued: the loop
    /// then takes the stimulus with no draw in between. Returns the slave the
    /// output was stopped on, for [`resume_output`].
    #[cfg(target_os = "linux")]
    fn stop_output_while_polling(&mut self, stimulus: impl FnOnce(&Self)) -> File {
        use std::os::unix::fs::OpenOptionsExt as _;
        let pid = self.child.id();
        let slave = std::fs::read_link(format!("/proc/{pid}/fd/1")).unwrap();
        let deadline = Instant::now() + PAINT_BOUND;
        loop {
            self.signal("STOP");
            while !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('T'))
            }) {
                assert!(
                    Instant::now() < deadline,
                    "the TUI never stopped on SIGSTOP"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.main_thread_polling() {
                break;
            }
            self.signal("CONT");
            let mut seen = Vec::new();
            self.drain(&mut seen);
            assert!(
                Instant::now() < deadline,
                "the TUI's frame loop was never caught in its event poll within {PAINT_BOUND:?}"
            );
        }
        let output = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&slave)
            .unwrap();
        // SAFETY: `output` is open for the call; TCOOFF only suspends its output.
        let stopped = unsafe { libc::tcflow(output.as_raw_fd(), libc::TCOOFF) };
        assert_ne!(stopped, -1, "tcflow: {}", std::io::Error::last_os_error());
        stimulus(self);
        self.signal("CONT");
        output
    }

    /// Set the input terminal's size, as a terminal window resize does.
    #[cfg(target_os = "linux")]
    fn resize(&self, rows: u16, cols: u16) {
        let master = self.input.as_ref().expect("the input terminal is open");
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one `winsize` that outlives the call.
        let set = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) };
        assert_ne!(set, -1, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
    }

    /// Reap the TUI without reading its screen, red if it is still running
    /// `EXIT_BOUND` after `after`.
    #[cfg(target_os = "linux")]
    fn wait_exit_unread(&mut self, after: &str) -> ExitStatus {
        let deadline = Instant::now() + EXIT_BOUND;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the TUI was still running {EXIT_BOUND:?} after {after}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// [`Tui::wait_exit`], also handing back everything the TUI drew from now
    /// to its exit: a live screen is read to its end after the reap, since the
    /// pty keeps what the TUI wrote before it exited.
    fn wait_exit_reading(&mut self, after: &str) -> (ExitStatus, Vec<u8>) {
        let deadline = Instant::now() + EXIT_BOUND;
        let mut seen = Vec::new();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                loop {
                    let before = seen.len();
                    self.drain(&mut seen);
                    if seen.len() == before {
                        return (status, seen);
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "the TUI was still running {EXIT_BOUND:?} after {after}"
            );
            // Keep a live terminal drained, so a write at exit never blocks
            // on a full pty buffer.
            if self.screen().is_some() {
                self.drain(&mut seen);
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// The live master the TUI draws to, if any.
    fn screen(&mut self) -> Option<&mut File> {
        match self.layout {
            Output::SameTerminal => self.input.as_mut(),
            Output::OwnTerminal => self.output.as_mut(),
        }
    }

    /// Read what the screen holds, waiting up to 100 ms for it.
    fn drain(&mut self, into: &mut Vec<u8>) {
        let Some(master) = self.screen() else {
            return;
        };
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`, alive for the whole call.
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready > 0 {
            let mut buf = [0u8; 65536];
            if let Ok(read) = master.read(&mut buf) {
                into.extend_from_slice(&buf[..read]);
            }
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run the TUI, close its terminal, and reap it, using only an attempt whose
/// TUI is still running [`WEDGED_AFTER`] past the close: one that exits sooner
/// left its frame loop on its own (the close landed between two event reads:
/// its stop check or a failed draw ended the loop), a path that never replays
/// the wedged read, and is held only to the same clean exit.
fn close_while_wedged(hangup: Hangup) -> (tempfile::TempDir, ExitStatus) {
    for _ in 0..ATTEMPTS {
        let home = tempfile::tempdir().unwrap();
        seed_credentials_link(home.path());
        let mut tui = Tui::spawn(home.path(), hangup);
        tui.wait_for_frame();
        tui.close_terminal();
        let closed = Instant::now();
        while closed.elapsed() < WEDGED_AFTER && tui.child.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(status) = tui.child.try_wait().unwrap() {
            assert_eq!(
                (status.code(), status.signal()),
                (Some(129), None),
                "a TUI that left its loop before the read wedged still exits through its \
                 shutdown, as 128 + SIGHUP: {status}"
            );
            assert_detached(home.path());
            continue;
        }
        let status = tui.wait_exit("its terminal closed");
        return (home, status);
    }
    panic!("no attempt's TUI was still in its event read {WEDGED_AFTER:?} after the close");
}

/// Point `~/.claude/.credentials.json` at a file through a symlink, as the
/// credential link does, so the shutdown's detach has a link to replace.
fn seed_credentials_link(home: &Path) {
    let claude = home.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let target = home.join("linked-credentials.json");
    std::fs::write(&target, "{}").unwrap();
    std::os::unix::fs::symlink(&target, claude.join(".credentials.json")).unwrap();
}

/// The shutdown ran: its credential detach replaced the link with a regular file.
fn assert_detached(home: &Path) {
    let link = home.join(".claude/.credentials.json");
    let kind = std::fs::symlink_metadata(&link).unwrap().file_type();
    assert!(
        kind.is_file(),
        "the shutdown detached the credential link into a regular file: {kind:?}"
    );
}

fn cloexec(fd: &OwnedFd) {
    // SAFETY: `fd` is open for the call; F_SETFD only sets its descriptor flags.
    let set = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    assert_ne!(set, -1, "fcntl: {}", std::io::Error::last_os_error());
}

#[test]
fn a_tui_whose_terminal_closes_exits_as_a_hangup() {
    let (home, status) = close_while_wedged(Hangup::Default);
    assert_eq!(
        (status.code(), status.signal()),
        (Some(129), None),
        "a closed terminal ends the TUI through its shutdown, exiting 128 + SIGHUP: {status}"
    );
    assert_detached(home.path());
}

/// A frame loop that leaves on its own over a dead terminal (its draw fails,
/// with no stop signal) still runs its shutdown, and its way out writes to
/// that dead terminal without crashing: an error exit, never an abort.
#[test]
fn a_tui_whose_output_terminal_dies_exits_with_an_error_not_a_crash() {
    let home = tempfile::tempdir().unwrap();
    seed_credentials_link(home.path());
    let mut tui = Tui::spawn_with(home.path(), Hangup::Default, Output::OwnTerminal);
    tui.wait_for_frame();
    tui.close_output();
    let status = tui.wait_exit("its output terminal closed");
    assert_eq!(
        (status.code(), status.signal()),
        (Some(1), None),
        "a draw failing on a dead terminal ends the TUI with an error exit: {status}"
    );
    assert_detached(home.path());
}

/// A TUI with no terminal left can do nothing, so one started under `nohup`
/// quits on the hangup too, rather than outliving its terminal.
#[test]
fn a_tui_started_under_nohup_exits_as_a_hangup_when_its_terminal_closes() {
    let (home, status) = close_while_wedged(Hangup::Ignored);
    assert_eq!(
        (status.code(), status.signal()),
        (Some(129), None),
        "a closed terminal ends a nohup'd TUI through its shutdown, exiting 128 + SIGHUP: {status}"
    );
    assert_detached(home.path());
}

#[test]
fn a_sigterm_on_a_live_terminal_exits_as_that_signal() {
    let home = tempfile::tempdir().unwrap();
    seed_credentials_link(home.path());
    let mut tui = Tui::spawn(home.path(), Hangup::Default);
    tui.wait_for_frame();
    tui.signal("TERM");
    let (status, screen) = tui.wait_exit_reading("a SIGTERM");
    assert_eq!(
        (status.code(), status.signal()),
        (Some(143), None),
        "a SIGTERM ends the TUI through its shutdown, exiting 128 + SIGTERM: {status}"
    );
    assert_detached(home.path());
    assert!(
        last(&screen, b"\x1b[?25h") > last(&screen, b"\x1b[?25l"),
        "the TUI hands the terminal back with its cursor shown"
    );
    assert!(
        last(&screen, b"\x1b[?1049l").is_some(),
        "the TUI hands the terminal back off the alternate screen"
    );
}

/// A terminal that stopped reading (a stalled ssh link, a frozen terminal
/// window) blocks the frame loop in a draw's write; a SIGTERM still ends the
/// TUI through its shutdown and exits, without waiting on that terminal.
#[cfg(target_os = "linux")]
#[test]
fn a_sigterm_on_a_terminal_that_stopped_reading_still_exits_as_that_signal() {
    let home = tempfile::tempdir().unwrap();
    seed_credentials_link(home.path());
    let mut tui = Tui::spawn(home.path(), Hangup::Default);
    tui.wait_for_frame();
    tui.stall_screen();
    tui.signal("TERM");
    let status = tui.wait_exit_unread("a SIGTERM over a terminal that stopped reading");
    assert_eq!(
        (status.code(), status.signal()),
        (Some(143), None),
        "a SIGTERM ends a TUI stuck writing to its terminal, exiting 128 + SIGTERM: {status}"
    );
    assert_detached(home.path());
}

/// Resume the output [`Tui::stop_output_while_polling`] stopped.
#[cfg(target_os = "linux")]
fn resume_output(output: &File) {
    // SAFETY: `output` is open for the call; TCOON only resumes its output.
    let resumed = unsafe { libc::tcflow(output.as_raw_fd(), libc::TCOON) };
    assert_ne!(resumed, -1, "tcflow: {}", std::io::Error::last_os_error());
}

/// A SIGTERM the frame loop sees at its own stop check, with no frame held,
/// stands the watcher down, so only the terminal hand-back's bound ends a TUI
/// whose terminal stopped taking output: it still exits as that signal.
#[cfg(target_os = "linux")]
#[test]
fn a_sigterm_seen_between_frames_over_stopped_output_still_exits_as_that_signal() {
    let home = tempfile::tempdir().unwrap();
    seed_credentials_link(home.path());
    let mut tui = Tui::spawn(home.path(), Hangup::Default);
    tui.wait_for_frame();
    let _output = tui.stop_output_while_polling(|tui| tui.signal("TERM"));
    let status = tui.wait_exit_unread("a SIGTERM between frames over stopped output");
    assert_eq!(
        (status.code(), status.signal()),
        (Some(143), None),
        "a SIGTERM seen between frames ends the TUI over stopped output, \
         exiting 128 + SIGTERM: {status}"
    );
    assert_detached(home.path());
    let log = std::fs::read_to_string(home.path().join(".clauth/clauth.log")).unwrap_or_default();
    assert!(
        log.contains("the terminal hand-back did not finish within"),
        "the hand-back's bound, not the watcher's fallback, ended the TUI"
    );
    assert!(
        log.contains("the terminal hand-back did not finish within 500ms"),
        "a stop signal's exit gives the hand-back 500 ms, not a quit's longer wait"
    );
}

/// A quit the user chose waits for a terminal whose output stalled, so a link
/// that recovers gets its screen back; it does not give up as fast as a stop
/// signal's exit does.
#[cfg(target_os = "linux")]
#[test]
fn a_quit_over_stalled_output_hands_the_terminal_back_once_it_resumes() {
    let home = tempfile::tempdir().unwrap();
    seed_credentials_link(home.path());
    let mut tui = Tui::spawn(home.path(), Hangup::Default);
    tui.wait_for_frame();
    // The first `q` arms the quit at the top level; its frame is drawn while
    // the output still flows.
    tui.send_keys(b"q");
    tui.keep_drained(Duration::from_secs(1));
    let output = tui.stop_output_while_polling(|tui| tui.send_keys(b"q"));
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        tui.child.try_wait().unwrap().is_none(),
        "a quit still waits for its terminal 2 s into a stalled output"
    );
    resume_output(&output);
    let (status, screen) = tui.wait_exit_reading("a quit whose terminal resumed");
    assert_eq!(
        (status.code(), status.signal()),
        (Some(0), None),
        "a quit exits cleanly once its terminal takes the hand-back: {status}"
    );
    assert_detached(home.path());
    assert!(
        last(&screen, b"\x1b[?1049l").is_some() && last(&screen, b"\x1b[?25h").is_some(),
        "the quit hands the terminal back off the alternate screen with its cursor shown"
    );
}

/// A quit over output that never resumes ends the TUI at the quit's own bound,
/// neither waiting on that terminal forever nor giving up at a stop signal's.
#[cfg(target_os = "linux")]
#[test]
fn a_quit_over_output_that_never_resumes_exits_at_the_quit_bound() {
    let home = tempfile::tempdir().unwrap();
    seed_credentials_link(home.path());
    let mut tui = Tui::spawn(home.path(), Hangup::Default);
    tui.wait_for_frame();
    tui.send_keys(b"q");
    tui.keep_drained(Duration::from_secs(1));
    let _output = tui.stop_output_while_polling(|tui| tui.send_keys(b"q"));
    let quit = Instant::now();
    let status = tui.wait_exit_unread("a quit over output that never resumes");
    let waited = quit.elapsed();
    assert_eq!(
        (status.code(), status.signal()),
        (Some(0), None),
        "a quit over a terminal that never takes the hand-back still exits cleanly: {status}"
    );
    assert_detached(home.path());
    let log = std::fs::read_to_string(home.path().join(".clauth/clauth.log")).unwrap_or_default();
    assert!(
        log.contains("the terminal hand-back did not finish within 10s"),
        "a quit gives a terminal that never resumes 10 s to take the hand-back"
    );
    // The 10 s wait starts only once the shutdown is done, so it is the floor;
    // the ceiling adds the shutdown's 4 s tick wait and 4 s of slack, and still
    // reds a wait twice the bound.
    assert!(
        (Duration::from_secs(10)..Duration::from_secs(18)).contains(&waited),
        "a quit over output that never resumes exits once its 10 s wait and the \
         shutdown are done: {waited:?}"
    );
}

/// Where `needle` last occurs in `bytes`.
fn last(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).rposition(|w| w == needle)
}
