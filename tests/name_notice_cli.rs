//! Stored account names the commands clauth prints cannot address, against the
//! real binary: a user-facing command names each one once on stderr, a machine
//! channel never does. Spawning is the only way to see what a caller captures.
//!
//! Unix only, for the reason `tests/oauth_token_cli.rs` gives: the child
//! resolves its home through `$HOME`, which only Unix lets a test point at a
//! sandbox.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

const DASH_NOTICE: &str = "clauth: -x: a name starting with '-' reads as a flag, so the commands \
                           clauth prints for it fail; rename it on the TUI's Setup tab";
const SUBCOMMAND_NOTICE: &str = "clauth: list: a bare `clauth list` runs clauth's own list command, \
                                 never a switch to this account; switch with `clauth switch list`, \
                                 or rename it on the TUI's Setup tab";
const CODEX_DASH_NOTICE: &str = "clauth: -c: a name starting with '-' reads as a flag, so the \
                                commands clauth prints for it fail; delete it and log in again \
                                under another name";
const CODEX_SUBCOMMAND_NOTICE: &str = "clauth: list: a bare `clauth list` runs clauth's own list \
                                       command, never a switch to this account; switch with \
                                       `clauth switch list`, or delete it and log in again under \
                                       another name";

fn seed(home: &Path, name: &str, config: &str) {
    let dir = home.join(".clauth").join("profiles").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
}

/// `-x` and `list` are hand-made blank accounts; `p2` is the active api-key one.
fn seed_roster(home: &Path) {
    seed(home, "-x", "");
    seed(home, "list", "");
    seed(
        home,
        "p2",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-p2-fixture\"\n",
    );
    std::fs::write(
        home.join(".clauth").join("profiles.toml"),
        "active_profile = \"p2\"\nprofiles = [\"-x\", \"list\", \"p2\"]\n",
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
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run clauth")
}

/// `clauth list` names each unaddressable account once, in roster order, and
/// nothing else reaches stderr.
#[test]
fn a_user_facing_command_names_each_unaddressable_account_once() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());

    let out = run(home.path(), &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stderr, format!("{DASH_NOTICE}\n{SUBCOMMAND_NOTICE}\n"));
}

/// A machine channel never carries either notice: `__api-key` prints the key
/// and nothing on stderr, and `__oauth-token`, which loads the whole roster,
/// refuses the keyed account with its own line and nothing else.
#[test]
fn the_credential_helpers_print_no_name_notice() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());

    let out = run(home.path(), &["__api-key", "p2"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "sk-p2-fixture");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "no notice on __api-key"
    );

    let out = run(home.path(), &["__oauth-token", "p2"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: profile 'p2' has no OAuth login\n",
        "no notice on __oauth-token"
    );
}

/// A machine channel that reads the CODEX roster stays quiet too: `__complete
/// --codex` prints the roster for the shell's completion and `__oauth-token`
/// refuses a codex name with its own line, each with no notice on stderr.
#[test]
fn a_codex_loading_machine_channel_prints_no_name_notice() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());
    std::fs::write(
        home.path().join(".clauth").join("codex-profiles.toml"),
        "profiles = [\"-c\", \"list\"]\n",
    )
    .unwrap();

    let out = run(home.path(), &["__complete", "--codex"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "__complete --codex reads the codex roster and prints no notice"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-c\nlist\n");

    let out = run(home.path(), &["__oauth-token", "--", "-c"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Error: '-c' is a codex profile; __oauth-token is claude-only\n",
        "__oauth-token loads the codex roster to refuse it and prints no notice"
    );
    assert_eq!(out.status.code(), Some(2));
}

/// A codex roster goes through the same load-notice machinery as the claude
/// one: its unaddressable names print once, in roster order.
#[test]
fn a_codex_roster_names_its_unaddressable_accounts_once() {
    let home = tempfile::tempdir().unwrap();
    seed(
        home.path(),
        "p2",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-p2-fixture\"\n",
    );
    std::fs::write(
        home.path().join(".clauth").join("profiles.toml"),
        "active_profile = \"p2\"\nprofiles = [\"p2\"]\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".clauth").join("codex-profiles.toml"),
        "profiles = [\"-c\", \"list\"]\n",
    )
    .unwrap();

    let out = run(home.path(), &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(
        stderr,
        format!("{CODEX_DASH_NOTICE}\n{CODEX_SUBCOMMAND_NOTICE}\n"),
        "each codex name prints once, in roster order"
    );
}

/// A name both rosters hold (a hand edit: creation refuses a cross-roster
/// duplicate) is one account to the CLI, as to the TUI and the daemon: it
/// prints one line, the claude account's, since that is the one a bare name
/// resolves to and the one the Setup tab can rename.
#[test]
fn a_name_in_both_rosters_prints_the_claude_line_once() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());
    std::fs::write(
        home.path().join(".clauth").join("codex-profiles.toml"),
        "profiles = [\"list\"]\n",
    )
    .unwrap();

    let out = run(home.path(), &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stderr, format!("{DASH_NOTICE}\n{SUBCOMMAND_NOTICE}\n"));
}

/// The same holds for a command that loads the codex roster before the claude
/// one (`sessions`): the shared name still prints the claude line, never the
/// codex copy whose "delete it" would aim at the claude account a bare name
/// resolves to. Its exit status is not this test's subject.
#[test]
fn a_name_in_both_rosters_prints_the_claude_line_when_codex_loads_first() {
    let home = tempfile::tempdir().unwrap();
    seed_roster(home.path());
    std::fs::write(
        home.path().join(".clauth").join("codex-profiles.toml"),
        "profiles = [\"list\"]\n",
    )
    .unwrap();

    let out = run(home.path(), &["sessions"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.lines().any(|line| line == SUBCOMMAND_NOTICE),
        "the claude line prints: {stderr}"
    );
    assert!(
        !stderr.lines().any(|line| line == CODEX_SUBCOMMAND_NOTICE),
        "the codex copy never prints for a name the claude roster holds: {stderr}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("clauth: list: "))
            .count(),
        1,
        "one line for the shared name: {stderr}"
    );
}

/// A stored name can hold bytes a name cannot: the notice prints each as its
/// escape, so no stored byte can drive the terminal or forge a line.
#[test]
fn an_unaddressable_name_holding_a_control_byte_prints_escaped() {
    let home = tempfile::tempdir().unwrap();
    seed(
        home.path(),
        "p2",
        "base_url = \"https://api.deepseek.com/anthropic\"\napi_key = \"sk-p2-fixture\"\n",
    );
    std::fs::write(
        home.path().join(".clauth").join("profiles.toml"),
        "active_profile = \"p2\"\nprofiles = [\"p2\"]\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".clauth").join("codex-profiles.toml"),
        "profiles = [\"-\\u001bred\"]\n",
    )
    .unwrap();

    let out = run(home.path(), &["list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(
        stderr,
        "clauth: -\\u{001b}red: a name starting with '-' reads as a flag, so the commands \
         clauth prints for it fail; delete it and log in again under another name\n"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "the raw escape never reaches stderr"
    );
}
