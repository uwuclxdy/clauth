//! `clauth rename` against the real binary: the record and the profile
//! directory move together, and each refusal exits 2 with nothing moved.
//!
//! Unix only, for the reason `tests/oauth_token_cli.rs` gives: the child
//! resolves its home through `$HOME`, which only Unix lets a test point at a
//! sandbox. Every rename here is of an inactive profile, so none reaches the
//! live credentials slot (or, on macOS, the Keychain).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

const API_KEY_CONFIG: &str =
    "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-fixture\"\n";

fn seed(home: &Path, name: &str) {
    let dir = home.join(".clauth").join("profiles").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), API_KEY_CONFIG).unwrap();
}

/// `main` is active, `spare` is the one each test renames, `cx` is codex.
fn seed_roster(home: &Path) {
    seed(home, "main");
    seed(home, "spare");
    std::fs::write(
        home.join(".clauth").join("profiles.toml"),
        "active_profile = \"main\"\nprofiles = [\"main\", \"spare\"]\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".clauth").join("codex-profiles.toml"),
        "profiles = [\"cx\"]\n",
    )
    .unwrap();
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("CLAUTH_NO_COMPLETIONS", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run clauth")
}

fn roster(home: &Path) -> String {
    std::fs::read_to_string(home.join(".clauth").join("profiles.toml")).unwrap()
}

fn profile_dir(home: &Path, name: &str) -> std::path::PathBuf {
    home.join(".clauth").join("profiles").join(name)
}

#[test]
fn rename_moves_the_record_and_the_directory() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());

    let out = run(home.path(), &["rename", "spare", "work"]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "clauth: renamed 'spare' to 'work'.\n"
    );
    let roster = roster(home.path());
    assert!(roster.contains("\"work\""), "roster: {roster}");
    assert!(!roster.contains("\"spare\""), "roster: {roster}");
    assert!(
        roster.contains("active_profile = \"main\""),
        "the active profile is untouched: {roster}"
    );
    assert_eq!(
        std::fs::read_to_string(profile_dir(home.path(), "work").join("config.toml")).unwrap(),
        API_KEY_CONFIG,
        "the profile's settings move with it"
    );
    assert!(!profile_dir(home.path(), "spare").exists());
}

#[test]
fn rename_refuses_and_moves_nothing() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["rename", "spare", "MAIN"],
            "Error: a profile named 'MAIN' already exists\n",
        ),
        (
            &["rename", "cx", "work"],
            "Error: 'cx' is a codex profile; rename is claude-only\n",
        ),
        (
            &["rename", "nope", "work"],
            "Error: profile 'nope' not found\navailable: main, spare\n",
        ),
    ];
    for (args, stderr) in cases {
        let home = tempfile::tempdir().unwrap();
        seed_roster(home.path());
        let before = roster(home.path());

        let out = run(home.path(), args);

        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), *stderr, "{args:?}");
        assert_eq!(
            roster(home.path()),
            before,
            "{args:?} left the roster alone"
        );
        assert!(profile_dir(home.path(), "spare").exists(), "{args:?}");
    }
}

#[test]
fn rename_to_the_same_name_is_a_no_op() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());
    let before = roster(home.path());

    let out = run(home.path(), &["rename", "spare", "spare"]);

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "clauth: 'spare' already has that name.\n"
    );
    assert_eq!(roster(home.path()), before);
}
