//! `clauth login px --setup-token --model <m>` whose plain `px` is deleted and
//! re-created as a proxy account while the token is being pasted: the landing
//! refuses with the re-login line (exit 2) and leaves the re-created account's
//! `config.toml` byte-for-byte, the `--model` edit included. Linux-only: the
//! wait for the child's stdin read reads `/proc/<pid>/syscall`.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROXY_CONFIG: &str = "base_url = \"http://127.0.0.1:9141\"\napi_key = \"clp_key\"\n\n[proxy]\nservice = \"stub\"\naccount = \"acct-1\"\n";

#[test]
fn a_setup_token_landing_onto_a_recreated_proxy_account_keeps_its_config() {
    let home = tempfile::tempdir().unwrap();
    let clauth = home.path().join(".clauth");
    let px = clauth.join("profiles").join("px");
    std::fs::create_dir_all(&px).unwrap();
    std::fs::write(clauth.join("profiles.toml"), "profiles = [\"px\"]\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["login", "px", "--setup-token", "--model", "r21-model"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A main thread blocked in read(0, ..) has loaded its config already.
    let syscall = format!("/proc/{}/syscall", child.id());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let s = std::fs::read_to_string(&syscall).unwrap_or_default();
        if s.starts_with("0 0x0 ") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the child never blocked on stdin: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir_all(&px).unwrap();
    std::fs::create_dir_all(&px).unwrap();
    std::fs::write(px.join("config.toml"), PROXY_CONFIG).unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"sk-ant-oat01-r21-setup-token-long-enough-to-pass-the-validator\n")
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let config = std::fs::read_to_string(px.join("config.toml")).unwrap();
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            config,
            px.join("session-token.json").exists(),
        ),
        (
            Some(2),
            String::new(),
            "Error: 'px' is an account on proxy \"stub\": log in to it again with `clauth login \
             px --proxy stub`, or delete it with `clauth delete px` first\n"
                .to_string(),
            PROXY_CONFIG.to_string(),
            false
        ),
        "(exit, stdout, stderr, px's config.toml, px's sidecar)"
    );
}
