//! `clauth devices` against the real binary: which stream carries what, and
//! what a signal or a newer code does to a waiting `pair`. Spawning is the only
//! way to see the bytes a shell would capture and the exit code it would get.
//!
//! Unix only, for the reason `tests/closed_reader.rs` gives: the child resolves
//! its home through `$HOME`, which only Unix lets a test point at a sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha2::Digest as _;

/// `clauth` with its home in `home` and nothing inherited that names another.
fn clauth(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clauth"));
    cmd.env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null());
    cmd
}

/// Start `clauth devices pair <name>` and read the code off its stdout, which
/// is printed only once the code is live.
fn start_pair(home: &Path, name: &str) -> (Child, BufReader<std::process::ChildStdout>, String) {
    let mut child = clauth(home)
        .args(["devices", "pair", name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clauth devices pair");
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read the code line");
    let code = line
        .strip_suffix('\n')
        .expect("the code ends its line")
        .to_string();
    (child, stdout, code)
}

/// The exit status, or a failure (and a killed child) past `limit`: a wait
/// that never ends must fail the test, not hang the suite.
fn wait_bounded(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("clauth devices pair did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    stderr
}

fn is_display_code(code: &str) -> bool {
    let alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let (head, tail) = code.split_once('-').unwrap_or(("", ""));
    head.len() == 4
        && tail.len() == 4
        && head
            .chars()
            .chain(tail.chars())
            .all(|c| alphabet.contains(c))
}

/// `add` puts the token alone on stdout, so `$(clauth devices add tray)`
/// captures exactly it, and its one-time warning on stderr. The list keeps the
/// token's digest, and a listing never prints either back.
#[test]
fn add_prints_the_token_alone_on_stdout() {
    let home = tempfile::tempdir().expect("home");
    let out = clauth(home.path())
        .args(["devices", "add", "tray", "--control"])
        .output()
        .expect("run clauth devices add");
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let token = stdout.strip_suffix('\n').unwrap_or_default();
    assert!(
        token.len() == 64
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "stdout must hold the token and nothing else"
    );
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf8 stderr"),
        "clauth: added device 'tray' (control). That token is its only copy: clauth keeps just \
         a SHA-256 of it and cannot show it again.\n"
    );

    let list = std::fs::read_to_string(home.path().join(".clauth/devices.json")).expect("list");
    assert!(!list.contains(token), "the list must hold no token");
    assert!(
        list.contains(&hex::encode(sha2::Sha256::digest(token.as_bytes()))),
        "the list holds the token's digest"
    );

    let listed = clauth(home.path())
        .args(["devices", "--json"])
        .output()
        .expect("run clauth devices --json");
    assert_eq!(listed.status.code(), Some(0));
    let listed = String::from_utf8(listed.stdout).expect("utf8");
    assert!(
        !listed.contains(token),
        "a listing never prints a token back"
    );
    let rows: serde_json::Value = serde_json::from_str(&listed).expect("json rows");
    assert_eq!(
        (&rows[0]["name"], &rows[0]["tier"], &rows[0]["joined"]),
        (
            &serde_json::json!("tray"),
            &serde_json::json!("control"),
            &serde_json::json!("add")
        )
    );
}

/// Ctrl-C during `pair` withdraws the code, then exits 130; stdout held the
/// code alone.
#[test]
fn ctrl_c_during_pair_withdraws_the_code_and_exits_130() {
    let home = tempfile::tempdir().expect("home");
    let (mut child, mut stdout, code) = start_pair(home.path(), "phone");
    assert!(is_display_code(&code), "stdout opens with the code alone");
    let pairing = home.path().join(".clauth/pairing.json");
    assert!(pairing.exists(), "the code is live once it is printed");

    let sent = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    let status = wait_bounded(&mut child, Duration::from_secs(10));

    assert_eq!(status.code(), Some(130), "Ctrl-C exits 130: {status:?}");
    assert!(!pairing.exists(), "Ctrl-C withdraws the code");
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("rest of stdout");
    assert_eq!(rest, "", "stdout carried the code and nothing else");
    let stderr = read_stderr(&mut child);
    assert!(
        stderr.ends_with("clauth: pairing code withdrawn\n"),
        "{stderr}"
    );
}

/// A newer `pair` replaces the waiting code: the replaced waiter says so and
/// exits 1, and the newer code is the one left live.
#[test]
fn a_replaced_pair_says_so_and_exits_1() {
    let home = tempfile::tempdir().expect("home");
    let (mut first, _first_out, first_code) = start_pair(home.path(), "phone");
    let (mut second, _second_out, second_code) = start_pair(home.path(), "tablet");
    assert_ne!(first_code, second_code);

    let status = wait_bounded(&mut first, Duration::from_secs(10));
    assert_eq!(status.code(), Some(1), "{status:?}");
    let stderr = read_stderr(&mut first);
    assert!(
        stderr.ends_with(
            "Error: a newer `clauth devices pair` replaced this code before anyone entered it\n"
        ),
        "{stderr}"
    );
    assert!(
        home.path().join(".clauth/pairing.json").exists(),
        "the replaced waiter leaves the newer code alone"
    );

    Command::new("kill")
        .args(["-INT", &second.id().to_string()])
        .status()
        .expect("run kill");
    assert_eq!(
        wait_bounded(&mut second, Duration::from_secs(10)).code(),
        Some(130)
    );
}

/// `clauth <args>` with stdout already closed: an OS pipe whose reader is
/// dropped before spawn, so the child's first stdout write meets `EPIPE`
/// instead of racing a reader that leaves later.
fn closed_stdout(home: &Path, args: &[&str]) -> std::process::Output {
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    clauth(home)
        .args(args)
        .stdout(Stdio::from(writer))
        .output()
        .expect("run clauth")
}

/// `clauth <args>` with stdout pointed at a socket whose send buffer is
/// already full and whose peer never reads, so every write fails with `EAGAIN`
/// instead of the `EPIPE` a closed pipe gives: the write-error arm the
/// closed-pipe tests never exercise. A full buffer is the one write fault that
/// fails the same way on every Unix runner — the Linux `/dev/full` has no
/// macOS twin, and a read-only handle's `EBADF` is what stdio swallows, not
/// what clauth sees.
fn full_stdout(home: &Path, args: &[&str]) -> std::process::Output {
    let (peer, writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    writer.set_nonblocking(true).expect("nonblocking write end");
    let mut filler = writer.try_clone().expect("clone the write end");
    // Fill the send buffer to capacity: a write that stops at `EAGAIN` has met
    // a full buffer, and a full buffer is what every later write meets too.
    let chunk = [0u8; 8192];
    loop {
        match filler.write(&chunk) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("fill the send buffer: {e}"),
        }
    }
    drop(filler);
    let out = clauth(home)
        .args(args)
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .output()
        .expect("run clauth");
    // `peer` stays open past the child's exit, so the child meets a full
    // buffer, not the `EPIPE` of a reader that left.
    drop(peer);
    out
}

/// True when `text` holds a run of 64 hex chars, the whole shape of a minted
/// token. Nothing else the command prints is that long of a hex run.
fn has_token_shape(text: &str) -> bool {
    text.as_bytes().windows(64).any(|w| {
        w.iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    })
}

/// True when `text` holds a `XXXX-XXXX` display-code shape.
fn has_code_shape(text: &str) -> bool {
    text.as_bytes()
        .windows(9)
        .any(|w| std::str::from_utf8(w).is_ok_and(is_display_code))
}

/// A reader gone before the token line prints loses nothing to keep: the run
/// revokes the device it just minted and exits 1, and the stderr names the loss
/// without the token itself.
#[test]
fn add_with_closed_stdout_rolls_back_and_exits_1() {
    let home = tempfile::tempdir().expect("home");
    let out = closed_stdout(home.path(), &["devices", "add", "tray"]);
    let status = out.status.code();
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert_eq!(status, Some(1), "{stderr}");
    assert!(
        stderr.contains("never reached its reader"),
        "stderr names the loss: {stderr}"
    );
    assert!(!has_token_shape(&stderr), "stderr holds no token: {stderr}");

    let listed = clauth(home.path())
        .args(["devices", "--json"])
        .output()
        .expect("run clauth devices --json");
    assert_eq!(listed.status.code(), Some(0));
    let listed = String::from_utf8(listed.stdout).expect("utf8");
    let rows: serde_json::Value = serde_json::from_str(&listed).expect("json rows");
    assert!(
        rows.as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["name"].as_str() != Some("tray"))),
        "the device was removed: {listed}"
    );
}

/// A reader gone before the code line prints leaves nothing to wait on: the
/// run withdraws the code and exits 1, so an `add` under the same name succeeds
/// right after.
#[test]
fn pair_with_closed_stdout_withdraws_and_exits_1() {
    let home = tempfile::tempdir().expect("home");
    let out = closed_stdout(home.path(), &["devices", "pair", "tray"]);
    let status = out.status.code();
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert_eq!(status, Some(1), "{stderr}");
    assert!(
        stderr.contains("never reached its reader"),
        "stderr names the loss: {stderr}"
    );
    assert!(!has_code_shape(&stderr), "stderr holds no code: {stderr}");

    let added = clauth(home.path())
        .args(["devices", "add", "tray"])
        .output()
        .expect("run clauth devices add");
    assert_eq!(
        added.status.code(),
        Some(0),
        "add succeeds once the code is withdrawn: {}",
        String::from_utf8_lossy(&added.stderr)
    );
}

/// A destination too full to take the token line is the same loss as a gone
/// reader, not a panic: the run revokes the device it just minted and exits 1,
/// and the stderr names the loss and the cause without the token itself.
#[test]
fn add_with_full_stdout_rolls_back_and_exits_1() {
    let home = tempfile::tempdir().expect("home");
    let out = full_stdout(home.path(), &["devices", "add", "tray"]);
    let status = out.status.code();
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert_eq!(status, Some(1), "{stderr}");
    assert!(
        stderr.contains("never reached its reader"),
        "stderr names the loss: {stderr}"
    );
    assert!(
        stderr.contains("never reached its reader ("),
        "stderr names the cause too: {stderr}"
    );
    assert!(!has_token_shape(&stderr), "stderr holds no token: {stderr}");

    let listed = clauth(home.path())
        .args(["devices", "--json"])
        .output()
        .expect("run clauth devices --json");
    assert_eq!(listed.status.code(), Some(0));
    let listed = String::from_utf8(listed.stdout).expect("utf8");
    let rows: serde_json::Value = serde_json::from_str(&listed).expect("json rows");
    assert!(
        rows.as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["name"].as_str() != Some("tray"))),
        "the device was removed: {listed}"
    );
}

/// A destination too full to take the code line withdraws the code and exits
/// 1, so an `add` under the same name succeeds right after.
#[test]
fn pair_with_full_stdout_withdraws_and_exits_1() {
    let home = tempfile::tempdir().expect("home");
    let out = full_stdout(home.path(), &["devices", "pair", "tray"]);
    let status = out.status.code();
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert_eq!(status, Some(1), "{stderr}");
    assert!(
        stderr.contains("never reached its reader"),
        "stderr names the loss: {stderr}"
    );
    assert!(
        stderr.contains("never reached its reader ("),
        "stderr names the cause too: {stderr}"
    );
    assert!(!has_code_shape(&stderr), "stderr holds no code: {stderr}");

    let added = clauth(home.path())
        .args(["devices", "add", "tray"])
        .output()
        .expect("run clauth devices add");
    assert_eq!(
        added.status.code(),
        Some(0),
        "add succeeds once the code is withdrawn: {}",
        String::from_utf8_lossy(&added.stderr)
    );
}

/// `add --control --sessions` still puts the token alone on stdout, and its
/// stderr names the grant. The stored row and `--json` both carry it.
#[test]
fn add_with_sessions_prints_the_token_and_names_the_grant() {
    let home = tempfile::tempdir().expect("home");
    let out = clauth(home.path())
        .args(["devices", "add", "tray", "--control", "--sessions"])
        .output()
        .expect("run clauth devices add");
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let token = stdout.strip_suffix('\n').unwrap_or_default();
    assert!(
        token.len() == 64
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "stdout must hold the token and nothing else"
    );
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf8 stderr"),
        "clauth: added device 'tray' (control). That token is its only copy: clauth keeps just \
         a SHA-256 of it and cannot show it again.\nclauth: 'tray' may mint sessions\n"
    );

    let list = std::fs::read_to_string(home.path().join(".clauth/devices.json")).expect("list");
    assert!(!list.contains(token), "the list must hold no token");

    let listed = clauth(home.path())
        .args(["devices", "--json"])
        .output()
        .expect("run clauth devices --json");
    assert_eq!(listed.status.code(), Some(0));
    let rows: serde_json::Value =
        serde_json::from_str(&String::from_utf8(listed.stdout).expect("utf8")).expect("json rows");
    assert_eq!(rows[0]["sessions"], serde_json::json!(true));
}

/// `allow-sessions` pins each of its four outcomes on the real binary: the
/// view refusal, the missing name, the fresh grant, and the already-granted
/// no-op.
#[test]
fn allow_sessions_prints_its_fixed_lines() {
    let home = tempfile::tempdir().expect("home");

    let view = clauth(home.path())
        .args(["devices", "add", "viewer"])
        .output()
        .expect("add a view device");
    assert_eq!(view.status.code(), Some(0));

    let refused = clauth(home.path())
        .args(["devices", "allow-sessions", "viewer"])
        .output()
        .expect("allow-sessions on a view device");
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(refused.stderr).expect("utf8 stderr"),
        "Error: a device without the control tier cannot mint sessions; revoke 'viewer' and \
         re-pair it with --control\n"
    );

    let missing = clauth(home.path())
        .args(["devices", "allow-sessions", "ghost"])
        .output()
        .expect("allow-sessions on a missing name");
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(missing.stderr).expect("utf8 stderr"),
        "Error: no device named 'ghost'; `clauth devices` lists the paired ones\n"
    );

    let control = clauth(home.path())
        .args(["devices", "add", "tray", "--control"])
        .output()
        .expect("add a control device");
    assert_eq!(control.status.code(), Some(0));

    let granted = clauth(home.path())
        .args(["devices", "allow-sessions", "tray"])
        .output()
        .expect("allow-sessions on a control device");
    assert_eq!(granted.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(granted.stdout).expect("utf8 stdout"),
        "clauth: 'tray' may now mint sessions\n"
    );

    let again = clauth(home.path())
        .args(["devices", "allow-sessions", "tray"])
        .output()
        .expect("allow-sessions again");
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(again.stdout).expect("utf8 stdout"),
        "clauth: 'tray' already may mint sessions\n"
    );
}

/// `--sessions` without `--control` is clap's own usage error on both minting
/// verbs.
#[test]
fn sessions_without_control_is_a_usage_error() {
    let home = tempfile::tempdir().expect("home");
    for verb in ["pair", "add"] {
        let out = clauth(home.path())
            .args(["devices", verb, "tray", "--sessions"])
            .output()
            .expect("run clauth devices");
        assert_eq!(out.status.code(), Some(2), "{verb}: {:?}", out.status);
        let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
        assert!(
            stderr.contains("--control"),
            "clap must name the missing --control: {stderr}"
        );
    }
}

/// Every `--json` row carries the sessions flag, granted or not.
#[test]
fn devices_json_carries_sessions_on_every_row() {
    let home = tempfile::tempdir().expect("home");
    let granted = clauth(home.path())
        .args(["devices", "add", "tray", "--control", "--sessions"])
        .output()
        .expect("add a granted device");
    assert_eq!(granted.status.code(), Some(0));
    let plain = clauth(home.path())
        .args(["devices", "add", "phone"])
        .output()
        .expect("add a view device");
    assert_eq!(plain.status.code(), Some(0));

    let listed = clauth(home.path())
        .args(["devices", "--json"])
        .output()
        .expect("run clauth devices --json");
    assert_eq!(listed.status.code(), Some(0));
    let rows: serde_json::Value =
        serde_json::from_str(&String::from_utf8(listed.stdout).expect("utf8")).expect("json rows");
    let rows = rows.as_array().expect("array");
    let tray = rows
        .iter()
        .find(|r| r["name"].as_str() == Some("tray"))
        .expect("tray");
    let phone = rows
        .iter()
        .find(|r| r["name"].as_str() == Some("phone"))
        .expect("phone");
    assert_eq!(tray["sessions"], serde_json::json!(true));
    assert_eq!(phone["sessions"], serde_json::json!(false));
}

/// `pair --control --sessions` says the grant on stderr before any device
/// exists, phrased conditionally like its control neighbour.
#[test]
fn pair_with_sessions_names_the_grant_before_any_device_exists() {
    let home = tempfile::tempdir().expect("home");
    let mut child = clauth(home.path())
        .args(["devices", "pair", "phone", "--control", "--sessions"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clauth devices pair");
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read the code line");
    assert!(
        is_display_code(line.strip_suffix('\n').unwrap_or_default()),
        "stdout opens with the code alone"
    );

    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert_eq!(
        wait_bounded(&mut child, Duration::from_secs(10)).code(),
        Some(130)
    );

    let stderr = read_stderr(&mut child);
    assert!(
        stderr.contains(
            "clauth: until it is used the code also grants sessions: whoever enters it first \
             can mint them through the API"
        ),
        "{stderr}"
    );
}

/// A revoke that drops the device but cannot read its push subscription row
/// reports the leftover row by its fixed sentence and exits 1: the device is
/// gone, the inert row stays behind, and the operator is told how to clear it.
#[test]
fn revoke_with_an_unreadable_subscription_store_reports_the_leftover_row() {
    let home = tempfile::tempdir().expect("home");
    let added = clauth(home.path())
        .args(["devices", "add", "phone"])
        .output()
        .expect("run clauth devices add");
    assert_eq!(added.status.code(), Some(0));

    // The device list write succeeds; the subscription store is a directory, so
    // the revoke's drop of the device's push row cannot read it.
    let push = home.path().join(".clauth/push-subscriptions.json");
    std::fs::create_dir_all(&push).expect("make the subscription store a directory");

    let out = clauth(home.path())
        .args(["devices", "revoke", "phone"])
        .output()
        .expect("run clauth devices revoke");
    assert_eq!(out.status.code(), Some(1), "{:?}", out.status);

    let expected = format!(
        "Error: revoked device 'phone', but its push subscription could not be dropped (failed to \
         read {}: Is a directory (os error 21)); it receives no push, since nothing is pushed to \
         a pairing the device list no longer holds, and the first subscription write after the \
         list reads again drops it; to clear it now, move ~/.clauth/push-subscriptions.json \
         aside, and every subscribed device then subscribes again\n",
        push.display()
    );
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf8 stderr"),
        expected
    );
}

/// A lost token line whose rollback drops the device but cannot read its push
/// subscription row reaches `revoke_lost`'s `SubscriptionLeft` arm: the leftover
/// row is named, and the loss (not the leftover) is the error. The round-1
/// `revoke_with_an_unreadable_subscription_store_reports_the_leftover_row`
/// pins the plain-`revoke` path this arm is not on.
#[test]
fn add_with_closed_stdout_reports_the_leftover_row_after_its_rollback() {
    let home = tempfile::tempdir().expect("home");
    // The subscription store is a directory BEFORE the add, so the rollback's
    // device drop succeeds while its push-row drop cannot read the store.
    let push = home.path().join(".clauth/push-subscriptions.json");
    std::fs::create_dir_all(&push).expect("make the subscription store a directory");

    let out = closed_stdout(home.path(), &["devices", "add", "tray"]);
    assert_eq!(out.status.code(), Some(1), "{:?}", out.status);

    let expected = format!(
        "clauth: revoked device 'tray', but its push subscription could not be dropped (failed to \
         read {}: Is a directory (os error 21)); it receives no push, since nothing is pushed to \
         a pairing the device list no longer holds, and the first subscription write after the \
         list reads again drops it; to clear it now, move ~/.clauth/push-subscriptions.json \
         aside, and every subscribed device then subscribes again\n\
         Error: the token for 'tray' never reached its reader; the device was removed\n",
        push.display()
    );
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf8 stderr"),
        expected
    );
}
