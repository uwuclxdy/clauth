//! `clauth rename` against the real binary: the record and the profile
//! directory move together, and a refusal of either name moves nothing.
//!
//! Unix only, for the reason `tests/oauth_token_cli.rs` gives: the child
//! resolves its home through `$HOME`, which only Unix lets a test point at a
//! sandbox. Every rename here is of an inactive profile, so none reaches the
//! live credentials slot (or, on macOS, the Keychain), except the Linux-only
//! relink case, whose live slot is a sandbox file.
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
    let cases: &[(&[&str], i32, &str)] = &[
        (
            &["rename", "spare", "MAIN"],
            1,
            "Error: a profile named 'MAIN' already exists\n",
        ),
        (
            &["rename", "cx", "work"],
            2,
            "Error: 'cx' is a codex profile; rename is claude-only\n",
        ),
        (
            &["rename", "nope", "work"],
            2,
            "Error: profile 'nope' not found\navailable: main, spare\n",
        ),
    ];
    for (args, code, stderr) in cases {
        let home = tempfile::tempdir().unwrap();
        seed_roster(home.path());
        let before = roster(home.path());

        let out = run(home.path(), args);

        assert_eq!(out.status.code(), Some(*code), "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), *stderr, "{args:?}");
        assert_eq!(
            roster(home.path()),
            before,
            "{args:?} left the roster alone"
        );
        assert!(profile_dir(home.path(), "spare").exists(), "{args:?}");
    }
}

/// A claude name reaches the codex roster only through the new name's check,
/// and a roster that fails to load there is a runtime failure, not bad usage.
#[test]
fn a_corrupt_codex_roster_fails_the_rename_as_a_runtime_error() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());
    std::fs::write(
        home.path().join(".clauth").join("codex-profiles.toml"),
        "profiles = [not toml",
    )
    .unwrap();
    let before = roster(home.path());

    let out = run(home.path(), &["rename", "spare", "work"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(roster(home.path()), before);
    assert!(profile_dir(home.path(), "spare").exists());
}

/// Renaming the ACTIVE profile while `~/.claude/.credentials.json` is a
/// regular file holding another login: the rename lands, the relink refuses,
/// and the command says both, exiting 1. Linux only: on macOS the active
/// rename settles the login Keychain, which no sandbox `$HOME` isolates.
#[cfg(target_os = "linux")]
#[test]
fn an_active_rename_whose_relink_is_refused_lands_and_exits_1() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());
    let login = |token: &str| format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}"}}}}"#);
    std::fs::write(
        profile_dir(home.path(), "main").join("credentials.json"),
        login("main-access"),
    )
    .unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join(".credentials.json"),
        login("someone-elses-access"),
    )
    .unwrap();

    let out = run(home.path(), &["rename", "main", "work"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: renamed 'main' to 'work', but the live credentials were not relinked\n\
         \n\
         Caused by:\n    \
         refusing to replace .credentials.json: live file differs from profile 'work'; \
         resolve the divergence in the `clauth` TUI first\n"
    );
    let roster = roster(home.path());
    assert!(
        roster.contains("active_profile = \"work\""),
        "the rename landed: {roster}"
    );
    assert!(
        profile_dir(home.path(), "work")
            .join("credentials.json")
            .exists()
    );
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

/// The `--` form the name notice prints for a dash-led account runs: clap
/// reads `-x` as the profile value and the rename lands on disk.
#[test]
fn a_dash_led_name_renames_through_the_double_dash_the_notice_prints() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "-x");
    std::fs::write(
        home.path().join(".clauth").join("profiles.toml"),
        "profiles = [\"-x\"]\n",
    )
    .unwrap();

    let out = run(home.path(), &["rename", "--", "-x", "fine"]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "clauth: renamed '-x' to 'fine'.\n"
    );
    let roster = roster(home.path());
    assert!(roster.contains("\"fine\""), "roster: {roster}");
    assert!(!roster.contains("\"-x\""), "roster: {roster}");
}

/// A stored name a hand edit gave a control byte still renames, and the
/// success line spells the byte escaped instead of feeding it to the terminal.
#[test]
fn a_rename_line_escapes_a_control_byte_in_the_old_name() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "a\u{1b}x");
    std::fs::write(
        home.path().join(".clauth").join("profiles.toml"),
        r#"profiles = ["a\u001bx"]"#,
    )
    .unwrap();

    let out = run(home.path(), &["rename", "a\u{1b}x", "b"]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "clauth: renamed 'a\\u{001b}x' to 'b'.\n"
    );
    assert!(roster(home.path()).contains("\"b\""));
}
