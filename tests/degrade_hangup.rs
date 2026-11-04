//! A degraded `clauth resume --with-fallback` killed by SIGHUP (a closed
//! terminal, herdr pane or ssh link) against the real binary: the launched
//! claude carries the degrade notice in its own env, and once the launcher is
//! gone a later prompt of the conversation from another process is not
//! refused with it. Only a real signal death of the real process shows what a
//! kill leaves behind.
//!
//! Linux only: the fake `claude` is a `/bin/sh` script whose env is read off
//! `/proc`, which macOS hides for a platform binary.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const NOTICE: &str = "clauth: resumed pinned to 'original', not following the fallback chain: \
                      'original' is no longer a fallback-chain member. This prompt was not sent: \
                      submit it again to continue on 'original'.";

/// Kills the launcher's whole process group if the test ends before it did.
struct Group(Child);

impl Drop for Group {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0.id())])
            .status();
        let _ = self.0.wait();
    }
}

fn clauth(home: &Path, bin: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clauth"));
    cmd.env_clear()
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    cmd
}

#[test]
fn a_hung_up_degraded_resume_leaves_no_notice_for_a_later_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // An OAuth account outside the (empty) fallback chain: a chain resume of
    // it degrades to pinned.
    let profile = home.join(".clauth/profiles/original");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("config.toml"), "").unwrap();
    std::fs::write(
        home.join(".clauth/profiles.toml"),
        "profiles = [\"original\"]\n",
    )
    .unwrap();
    let projects = home.join(".claude/projects/-w");
    std::fs::create_dir_all(&projects).unwrap();
    let line = serde_json::json!({
        "sessionId": "known-session",
        "cwd": workspace.to_string_lossy(),
        "message": {"role": "user", "content": "hi"},
    });
    std::fs::write(projects.join("known-session.jsonl"), format!("{line}\n")).unwrap();

    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let seen = tmp.path().join("seen");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" --resume \"*) printf '%s\\n%s\\n' \"$$\" \"${{CLAUTH_DEGRADE_NOTICE-}}\" > '{seen}.tmp'; mv '{seen}.tmp' '{seen}'; exec sleep 60;;\n  *--version*) echo '2.1.317 (Claude Code)';;\nesac\nexit 0\n",
            seen = seen.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let launcher = Group(
        clauth(&home, &bin)
            .args([
                "resume",
                "known-session",
                "--profile",
                "original",
                "--with-fallback",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("run clauth resume"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !seen.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let record = std::fs::read_to_string(&seen).expect("the resumed claude started");
    let mut lines = record.lines();
    let claude_pid = lines.next().unwrap().to_string();
    assert_eq!(
        lines.next(),
        Some(NOTICE),
        "the launched claude carries the degrade notice in its env"
    );

    let mut launcher = launcher;
    let status = Command::new("kill")
        .args(["-HUP", "--", &format!("-{}", launcher.0.id())])
        .status()
        .unwrap();
    assert!(status.success());
    let exit = launcher.0.wait().unwrap();
    assert_eq!(exit.signal(), Some(1), "clauth died by SIGHUP: {exit:?}");

    // The conversation's note record holds no notice for anyone: a later
    // process's take could otherwise be the only thing standing between it
    // and a false refusal.
    let record = home.join(".clauth/conversations/known-session.json");
    if let Ok(bytes) = std::fs::read(&record) {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value
                .get("restore_notice")
                .is_none_or(serde_json::Value::is_null),
            "the hung-up resume left a notice on the record"
        );
    }

    // Another process of the conversation, later: no degrade env, a pid that
    // is not the hung-up claude's.
    let mut hook = clauth(&home, &bin)
        .arg("hook-profile-changed-note")
        .env("CLAUDE_PID", std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the hook");
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"UserPromptSubmit","session_id":"known-session"}"#)
        .unwrap();
    let out = hook.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "the hook fire failed");
    assert!(
        !stdout.contains("\"decision\""),
        "a later prompt from another process was refused after claude {claude_pid} hung up: {stdout}"
    );
}
