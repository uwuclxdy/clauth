//! `clauth __restore <sid>` against the real binary: on unix it `exec`s the
//! `claude` it resolves, so the pane's process is claude itself (same pid,
//! same exit code), resuming the conversation with the marker naming it and
//! that pid. Spawning is the only way to see the process a pane ends up
//! running.
//!
//! Unix only: the `exec` arm is the unix one, and the fake `claude` is a
//! `/bin/sh` script.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn a_restore_execs_claude_with_the_marker_naming_its_own_pid() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$$\" \"$CLAUTH_HERDR_RESTORE\" \"$*\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["__restore", "sidx"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run clauth");
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{pid}|sidx:{pid}|--resume sidx\n"),
        "stderr: {stderr}"
    );
    assert_eq!(out.status.code(), Some(7), "stderr: {stderr}");
    assert_eq!(stderr, "");
}
