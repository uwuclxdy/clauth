//! `clauth __oauth-token <profile>` against the real binary: the exact bytes on
//! stdout, what an endpoint profile gets instead, and the exit codes a script
//! branches on. Spawning is the only way to see what a caller captures.
//!
//! No fixture carries a refresh token, so no run here can reach a token
//! endpoint, whatever the code under test decides; the refresh leg is pinned
//! in-process against a loopback stub (`tests/inline/cli.rs`).
//!
//! Unix only, for the reason `tests/closed_reader.rs` gives: the child resolves
//! its home through `$HOME`, which only Unix lets a test point at a sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// 2100-01-01, far outside any refresh window.
const FAR_EXPIRY_MS: i64 = 4_102_444_800_000;

/// Register `name` in the sandbox roster and give it a profile dir.
fn seed_profile(home: &Path, name: &str) -> std::path::PathBuf {
    let clauth = home.join(".clauth");
    let dir = clauth.join("profiles").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        clauth.join("profiles.toml"),
        format!("profiles = [\"{name}\"]\n"),
    )
    .unwrap();
    dir
}

/// A refresh-less OAuth login holding `access_token`.
fn login_json(access_token: &str) -> String {
    format!(r#"{{"claudeAiOauth":{{"accessToken":"{access_token}","expiresAt":{FAR_EXPIRY_MS}}}}}"#)
}

fn run_oauth_token(home: &Path, name: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["__oauth-token", name])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run clauth __oauth-token")
}

/// A live OAuth login prints its access token and nothing else: no newline,
/// no stderr, exit 0.
#[test]
fn oauth_token_prints_the_access_token_and_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "acme");
    std::fs::write(dir.join("credentials.json"), login_json("at-fixture-live")).unwrap();

    let out = run_oauth_token(home.path(), "acme");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-fixture-live".to_vec());
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

/// A profile with a long-lived `session-token.json` runs its sessions on that
/// token, so that is what prints, never the usage pair in `credentials.json`.
#[test]
fn oauth_token_prints_the_session_token_of_a_split_profile() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "split");
    std::fs::write(dir.join("credentials.json"), login_json("at-usage-pair")).unwrap();
    std::fs::write(
        dir.join("session-token.json"),
        login_json("at-long-lived-mint"),
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "split");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-long-lived-mint".to_vec());
}

/// A refresh-less login expiring at `expires_at`.
fn login_json_expiring(access_token: &str, expires_at: i64) -> String {
    format!(r#"{{"claudeAiOauth":{{"accessToken":"{access_token}","expiresAt":{expires_at}}}}}"#)
}

/// Epoch-ms `minutes` from now.
fn minutes_from_now(minutes: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    now + minutes * 60_000
}

/// The stderr both arms print for a `split` setup token inside the five-minute
/// window: the gate's re-mint line, then the re-login error.
fn split_expiry_stderr() -> &'static str {
    "clauth: 'split' long-lived token has expired (or sits inside Claude Code's own five-minute refresh window). Re-mint with `claude setup-token` (clauth login split --setup-token)\nError: login for 'split' has expired: refresh token revoked or invalid: run `clauth login split`\n"
}

fn profiles_toml(home: &Path) -> String {
    std::fs::read_to_string(home.join(".clauth").join("profiles.toml")).unwrap()
}

/// Register `name` as the sandbox's globally active profile.
fn seed_active(home: &Path, name: &str) -> std::path::PathBuf {
    let dir = seed_profile(home, name);
    std::fs::write(
        home.join(".clauth").join("profiles.toml"),
        format!("active_profile = \"{name}\"\nprofiles = [\"{name}\"]\n"),
    )
    .unwrap();
    dir
}

/// Point `~/.claude/.credentials.json` at `target` the way a switch links it.
fn link_live_slot(home: &Path, target: &Path) {
    let claude = home.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::os::unix::fs::symlink(target, claude.join(".credentials.json")).unwrap();
}

/// Seed `name` as the active profile with an expired, refresh-less store login
/// (`at-store-stale`) and a live `~/.claude/.credentials.json` that a refresh
/// replaced as a regular file holding `live_token`. Identity is CC's cached
/// `~/.claude.json` `oauthAccount.accountUuid` (`live_uuid`) against the
/// profile's `account_id.json` anchor (`acct-live`), the divergence path's own
/// check: the credentials file carries no account of its own.
fn seed_active_with_live_login(home: &Path, name: &str, live_token: &str, live_uuid: &str) {
    let dir = seed_active(home, name);
    std::fs::write(
        dir.join("credentials.json"),
        login_json_expiring("at-store-stale", 1_000_000_000_000),
    )
    .unwrap();
    std::fs::write(dir.join("account_id.json"), "\"acct-live\"").unwrap();
    let claude = home.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join(".credentials.json"), login_json(live_token)).unwrap();
    std::fs::write(
        home.join(".claude.json"),
        format!(r#"{{"oauthAccount":{{"accountUuid":"{live_uuid}"}}}}"#),
    )
    .unwrap();
}

/// The globally active profile prints what a plain `claude` last wrote, never
/// the store's superseded copy, once the live login is proven its own, and is
/// never refreshed: the store's login is expired and refresh-less, so a refresh
/// attempt would end in the dead-chain refusal (exit 1) instead of reaching any
/// endpoint.
#[test]
fn oauth_token_prints_the_active_profiles_live_token_without_refreshing() {
    let home = tempfile::tempdir().unwrap();
    seed_active_with_live_login(home.path(), "live", "at-live-fresh", "acct-live");

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-live-fresh".to_vec());
}

/// A live login from another account under the active profile's name is
/// refused with the divergence wording, and its token never reaches stdout.
#[test]
fn oauth_token_refuses_an_active_profiles_live_login_from_another_account() {
    let home = tempfile::tempdir().unwrap();
    seed_active_with_live_login(home.path(), "live", "at-other-account", "acct-other");

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: 'live' has a login clauth hasn't saved, resolve the divergence in the `clauth` TUI\n"
    );
}

/// An active split profile whose live slot still links to its own sidecar is
/// the profile's own without any identity record: the mint prints, though no
/// `account_id.json` anchor and no `~/.claude.json` exist to prove it.
#[test]
fn oauth_token_prints_an_active_split_profiles_linked_mint() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "split");
    std::fs::write(dir.join("credentials.json"), login_json("at-usage-pair")).unwrap();
    std::fs::write(
        dir.join("session-token.json"),
        login_json("at-long-lived-mint"),
    )
    .unwrap();
    link_live_slot(home.path(), &dir.join("session-token.json"));

    let out = run_oauth_token(home.path(), "split");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-long-lived-mint".to_vec());
}

/// The setup token wins even for the active profile: an active split profile
/// whose live slot still points at `credentials.json` prints the long-lived
/// sidecar, never the rotating usage pair the live slot holds (cloudy
/// 2026-10-13: "year-long token wins").
#[test]
fn oauth_token_prints_the_setup_token_of_an_active_split_profile() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "split");
    std::fs::write(dir.join("credentials.json"), login_json("at-usage-pair")).unwrap();
    std::fs::write(
        dir.join("session-token.json"),
        login_json("at-long-lived-mint"),
    )
    .unwrap();
    link_live_slot(home.path(), &dir.join("credentials.json"));

    let out = run_oauth_token(home.path(), "split");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-long-lived-mint".to_vec());
}

/// The active profile's setup token obeys the same five-minute window as the
/// inactive arm and the switch gate: a mint inside it is refused with the
/// gate's re-mint line then the re-login error, exactly as a switch would.
#[test]
fn oauth_token_refuses_an_active_profiles_setup_token_inside_the_five_minute_window() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "split");
    std::fs::write(
        dir.join("session-token.json"),
        login_json_expiring("at-mint", minutes_from_now(3)),
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "split");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(String::from_utf8_lossy(&out.stderr), split_expiry_stderr());
}

/// The inactive arm prints the identical stderr on identical bytes, so both
/// arms are pinned against the same string.
#[test]
fn oauth_token_refuses_an_inactive_profiles_setup_token_inside_the_five_minute_window() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "split");
    std::fs::write(
        dir.join("session-token.json"),
        login_json_expiring("at-mint", minutes_from_now(3)),
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "split");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(String::from_utf8_lossy(&out.stderr), split_expiry_stderr());
}

/// A mint with ten minutes left is outside the five-minute window: it prints,
/// even for the active profile.
#[test]
fn oauth_token_prints_an_active_profiles_setup_token_with_ten_minutes_left() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "split");
    std::fs::write(
        dir.join("session-token.json"),
        login_json_expiring("at-mint", minutes_from_now(10)),
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "split");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-mint".to_vec());
}

/// An active profile with no live credentials file has nothing newer than its
/// store, so the store's token prints, unrefreshed.
#[test]
fn oauth_token_prints_the_active_profiles_store_token_with_no_live_login() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "live");
    std::fs::write(dir.join("credentials.json"), login_json("at-store-good")).unwrap();

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-store-good".to_vec());
}

/// Seed an active profile with an expired store login and no live file.
fn seed_active_expired_store_without_live_file(home: &Path, name: &str) {
    let dir = seed_active(home, name);
    std::fs::write(
        dir.join("credentials.json"),
        login_json_expiring("at-store-expired", 1_000_000_000_000),
    )
    .unwrap();
}

/// Off macOS a missing live file means no `claude` holds the active chain, so
/// an expired stored token gets the re-login line, not the `claude`-run one.
#[cfg(not(target_os = "macos"))]
#[test]
fn oauth_token_refuses_an_active_profiles_expired_store_token_with_no_live_file() {
    let home = tempfile::tempdir().unwrap();
    seed_active_expired_store_without_live_file(home.path(), "live");

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: login for 'live' has expired: refresh token revoked or invalid: run `clauth login live`\n"
    );
}

/// On macOS the active login lives in the Keychain item, so the same input
/// keeps the line that names the `claude` run that refreshes it.
#[cfg(target_os = "macos")]
#[test]
fn oauth_token_refuses_an_active_profiles_expired_store_token_with_no_live_file() {
    let home = tempfile::tempdir().unwrap();
    seed_active_expired_store_without_live_file(home.path(), "live");

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: 'live': its token has expired and a running `claude` owns its refresh, run `claude` on it once and retry\n"
    );
}

/// An active profile's expired live token never prints: exit 1, stdout empty,
/// with the line that names the `claude` run that refreshes it.
#[test]
fn oauth_token_refuses_an_active_profiles_expired_live_token() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "live");
    std::fs::write(
        dir.join("credentials.json"),
        login_json_expiring("at-expired-live", 1_000_000_000_000),
    )
    .unwrap();
    link_live_slot(home.path(), &dir.join("credentials.json"));

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: 'live': its token has expired and a running `claude` owns its refresh, run `claude` on it once and retry\n"
    );
}

/// Claude Code's logged-out shell in the live slot (blank tokens, written when
/// its own refresh died) is a dead login, not an absent one: exit 1 with the
/// re-login line, stdout empty, and the store's copy of that chain never
/// prints.
#[test]
fn oauth_token_refuses_an_active_profiles_logged_out_shell() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_active(home.path(), "live");
    std::fs::write(dir.join("credentials.json"), login_json("at-store-good")).unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0}}"#,
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "live");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: login for 'live' has expired: refresh token revoked or invalid: run `clauth login live`\n"
    );
}

/// A login with no refresh token and 30 minutes left, inside the command's
/// hour, has nothing to refresh it: it prints as-is and is never quarantined.
#[test]
fn oauth_token_prints_an_unexpired_login_with_no_refresh_token() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "acme");
    std::fs::write(
        dir.join("credentials.json"),
        login_json_expiring("at-thirty-min", minutes_from_now(30)),
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "acme");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"at-thirty-min".to_vec());
    assert_eq!(profiles_toml(home.path()), "profiles = [\"acme\"]\n");
}

/// The same login once expired exits 1 with the shared re-login line alone, and
/// the profile is still not quarantined.
#[test]
fn oauth_token_refuses_an_expired_login_with_no_refresh_token_without_quarantine() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "acme");
    std::fs::write(
        dir.join("credentials.json"),
        login_json_expiring("at-expired", 1_000_000_000_000),
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "acme");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: login for 'acme' has expired: refresh token revoked or invalid: run `clauth login acme`\n"
    );
    assert_eq!(profiles_toml(home.path()), "profiles = [\"acme\"]\n");
}

/// An endpoint profile has no OAuth login: exit 1, nothing on stdout (its key
/// least of all), and stderr naming the profile.
#[test]
fn oauth_token_refuses_an_endpoint_profile_with_empty_stdout() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "keyed");
    std::fs::write(
        dir.join("config.toml"),
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-fixture-key\"\n",
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "keyed");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: profile 'keyed' has no OAuth login\n"
    );
}

/// A hand-edited store can poison the access token with a control character,
/// which CC would forward verbatim as a bearer: the command refuses with exit
/// 1, empty stdout, and the same noun-correct line the api-key twin uses.
#[test]
fn oauth_token_refuses_a_token_holding_a_control_character() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "acme");
    std::fs::write(
        dir.join("credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"at\ninjected","expiresAt":4102444800000}}"#,
    )
    .unwrap();

    let out = run_oauth_token(home.path(), "acme");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: oauth token contains whitespace or control characters, likely a bad paste or an edited config\n"
    );
}

/// A script that closes its read end before the command prints still gets a
/// failure the exit code reflects, and the error names the OAuth token, never
/// an api key.
#[test]
fn oauth_token_write_error_names_the_oauth_token() {
    let home = tempfile::tempdir().unwrap();
    let dir = seed_profile(home.path(), "acme");
    std::fs::write(dir.join("credentials.json"), login_json("at-fixture-live")).unwrap();

    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["__oauth-token", "acme"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUTH_NO_UPDATE", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .output()
        .expect("run clauth __oauth-token");

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, Vec::<u8>::new());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: flushing oauth token to stdout\n\nCaused by:\n    Broken pipe (os error 32)\n"
    );
}
