//! `clauth static-token <name> --clear --yes` against the real binary: pins
//! that the report names only what the clear removed. The postscripts' own
//! gating is pinned in-process (`tests/inline/cli.rs`,
//! `the_clear_postscripts_ride_exactly_what_moved`); the headline and the
//! failure path print from `cmd_static_token_clear` itself, pinned here on the
//! real binary's stdout.
//!
//! Unix only, for the reason `tests/closed_reader.rs` gives: the child resolves
//! its home through `$HOME`, which only Unix lets a test point at a sandbox.
//! Off macOS too, per `tests/oauth_token_cli.rs`'s precedent: an `env_clear()`ed
//! child there has no `USER` for the Keychain arm.
#![cfg(all(unix, not(target_os = "macos")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn seed(home: &Path, name: &str, config: &str) -> PathBuf {
    let dir = home.join(".clauth/profiles").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
    std::fs::write(
        home.join(".clauth/profiles.toml"),
        format!("profiles = [\"{name}\"]\n"),
    )
    .unwrap();
    dir
}

fn run_clear(home: &Path, name: &str) -> Output {
    run_clear_with(home, name, &["--yes"])
}

fn run_clear_with(home: &Path, name: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["static-token", name, "--clear"])
        .args(extra)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run clauth static-token --clear")
}

/// Only the real binary runs the terminal check: a test build reads the
/// confirm seam instead.
#[test]
fn a_clear_off_a_terminal_without_yes_refuses() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed(
        home.path(),
        "acme",
        "api_key = \"sk-acme-fixture\"\nrolling_token = true\n",
    );
    let sidecar = dir.join("session-token.json");
    std::fs::write(
        &sidecar,
        r#"{"claudeAiOauth":{"accessToken":"mint-access","refreshToken":null,"expiresAt":null}}"#,
    )
    .unwrap();

    let out = run_clear_with(home.path(), "acme", &[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: pass --yes to clear the long-lived token for 'acme' non-interactively\n"
    );
    assert!(sidecar.exists(), "a refused clear leaves the token on disk");
}

/// A flag-only account: no sidecar, no preserved mint, only `rolling_token`.
/// The clear turns the flag off and removes no token, so the headline says
/// none was in use; the disarm postscript still reports the flag.
#[test]
fn a_flag_only_clear_names_no_removed_token() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "solo", "rolling_token = true\n");

    let out = run_clear(home.path(), "solo");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            "clauth: found no long-lived token in use for 'solo'. It stores no login at all \
             now. Run `clauth login solo` before switching to it.",
            "clauth: rolling-token is off. Nothing re-stamps a sidecar for 'solo' now.",
        ],
        "stdout: {stdout}"
    );
}

/// A sidecar the clear DOES remove keeps the old opener: the headline gate
/// names a removed token wherever one was removed.
#[test]
fn a_clear_that_removes_the_sidecar_names_the_cleared_token() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed(
        home.path(),
        "acme",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-acme-fixture\"\n",
    );
    let sidecar = dir.join("session-token.json");
    std::fs::write(
        &sidecar,
        r#"{"claudeAiOauth":{"accessToken":"mint-access","refreshToken":null,"expiresAt":null}}"#,
    )
    .unwrap();

    let out = run_clear(home.path(), "acme");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!sidecar.exists(), "fixture: the clear removed the sidecar");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            "clauth: cleared the long-lived token for 'acme'. It stores no OAuth login, so \
          switching to it authenticates by api key:  clauth switch acme"
        ],
        "stdout: {stdout}"
    );
}

/// The flag persist lands, then the preserved mint's removal fails (the slot
/// is a non-empty directory, so `remove_file` errors). The flag is already
/// off on disk, so the run must still say re-stamping stopped before it
/// reports the failure.
#[test]
fn a_failed_backup_removal_still_reports_the_disarm() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed(
        home.path(),
        "acme",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-acme-fixture\"\n\
         rolling_token = true\n",
    );
    let backup = dir.join("session-token.static.json");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("pin"), "x").unwrap();

    let out = run_clear(home.path(), "acme");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "the failed removal is a failure: stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout
            .lines()
            .any(|l| l
                == "clauth: rolling-token is off. Nothing re-stamps a sidecar for 'acme' now."),
        "the disarm landed on disk and must be reported: stdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|l| *l
                == "clauth: rolling-token is off. Nothing re-stamps a sidecar for 'acme' now.")
            .count(),
        1,
        "the disarm is reported exactly once: stdout: {stdout}"
    );
    let config = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(
        !config.contains("rolling_token = true"),
        "fixture: the flag persist landed before the failure: {config}"
    );
}

/// The same failure, whole line: the backup-removal error names the mint that
/// remains and opens on what the clear found. Only this first line is pinned —
/// the cause lines after it carry the OS's own error text, which differs per
/// platform.
#[test]
fn a_failed_backup_removal_names_the_mint_that_remains() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed(
        home.path(),
        "acme",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-acme-fixture\"\n\
         rolling_token = true\n",
    );
    let backup = dir.join("session-token.static.json");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("pin"), "x").unwrap();

    let out = run_clear(home.path(), "acme");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "the failed removal is a failure: stderr: {stderr}"
    );
    assert_eq!(
        stderr.lines().next(),
        Some(
            "Error: found no long-lived token in use for 'acme', but the preserved mint at \
             session-token.static.json remains. Remove it by hand, or re-run"
        ),
        "stderr: {stderr}"
    );
}

/// The sidecar holds a rotating pair (a mis-fill), not a long-lived token: the
/// clear moves it to quarantine and removes no long-lived token, so the opener
/// says none was in use. Gating the opener on "a sidecar was present" instead
/// of on the removal itself would print "cleared" here.
#[test]
fn a_quarantined_misfill_names_no_removed_token() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed(
        home.path(),
        "mf",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-mf-fixture\"\n",
    );
    std::fs::write(
        dir.join("session-token.json"),
        "{\"claudeAiOauth\": {\"accessToken\": \"at-misfill\", \"refreshToken\": \"rt-misfill\", \
         \"expiresAt\": 4102444800000}}",
    )
    .unwrap();

    let out = run_clear(home.path(), "mf");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            "clauth: found no long-lived token in use for 'mf'. It stores no OAuth login, so \
             switching to it authenticates by api key:  clauth switch mf"
        ],
        "stdout: {stdout}"
    );
    assert!(
        !dir.join("session-token.json").exists(),
        "fixture: the mis-fill left the sidecar slot"
    );
    let quarantined = std::fs::read_dir(dir.join("quarantine"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with(".session-token.json")
        })
        .count();
    assert_eq!(quarantined, 1, "fixture: the mis-fill went to quarantine");
}

/// The flag persist lands, then the sidecar removal itself fails (the slot is
/// a non-empty directory). Every failure after the persist must still say
/// re-stamping stopped: the flag is already off, so a re-run finds nothing to
/// clear and cannot say it.
#[test]
fn a_failed_sidecar_removal_still_reports_the_disarm() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed(
        home.path(),
        "acme",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-acme-fixture\"\n\
         rolling_token = true\n",
    );
    let sidecar = dir.join("session-token.json");
    std::fs::create_dir_all(&sidecar).unwrap();
    std::fs::write(sidecar.join("pin"), "x").unwrap();

    let out = run_clear(home.path(), "acme");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "the failed removal is a failure: stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout
            .lines()
            .any(|l| l
                == "clauth: rolling-token is off. Nothing re-stamps a sidecar for 'acme' now."),
        "the disarm landed on disk and must be reported: stdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|l| *l
                == "clauth: rolling-token is off. Nothing re-stamps a sidecar for 'acme' now.")
            .count(),
        1,
        "the disarm is reported exactly once: stdout: {stdout}"
    );
    let config = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(
        !config.contains("rolling_token = true"),
        "fixture: the flag persist landed before the failure: {config}"
    );
}
