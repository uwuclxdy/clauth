use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::testutil::{HomeSandbox, set_mtime};

/// Deterministic timestamps so mtime ordering is unambiguous in tests.
fn t(offset: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + offset)
}

fn write_json(path: &Path, value: &Value, when: SystemTime) {
    #[allow(clippy::expect_used)]
    fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
    fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
    set_mtime(path, when);
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("parse")
}

fn base_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

fn runtime_path(home: &Path, profile: &str) -> PathBuf {
    home.join(".clauth/profiles")
        .join(profile)
        .join("runtime")
        .join("settings.json")
}

fn isolated_path(home: &Path, profile: &str) -> PathBuf {
    home.join(".clauth/profiles")
        .join(profile)
        .join("runtime-isolated")
        .join("settings.json")
}

fn write_config(home: &Path, profile: &str, body: &str) {
    let dir = home.join(".clauth/profiles").join(profile);
    fs::create_dir_all(&dir).expect("mkdir profile");
    fs::write(dir.join("config.toml"), body).expect("write config.toml");
}

/// One reconciliation over the real member list, exactly as `sync_once` runs it
/// minus the `LAST_SYNCED` fast path (a process-wide static that would make test
/// order significant).
fn sync() {
    let paths = known_paths().expect("known paths");
    assert!(
        sync_members(&paths).expect("sync"),
        "the merge must have run; a paused tick means a config.toml did not parse"
    );
}

#[test]
fn shared_field_reaches_the_base_and_every_sibling_runtime() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    write_json(&p1, &json!({"theme": "dark", "env": {}}), t(10));
    write_json(&p2, &json!({"theme": "light", "env": {}}), t(5));

    sync();

    assert_eq!(read_json(&base)["theme"], json!("dark"));
    assert_eq!(read_json(&p2)["theme"], json!("dark"));
    assert_eq!(read_json(&p1)["theme"], json!("dark"), "winner untouched");
}

#[test]
fn per_profile_env_never_leaves_its_own_runtime() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_config(home.home(), "p1", "[env]\nMY_TOKEN = \"p1-secret\"\n");
    write_config(home.home(), "p2", "");
    write_json(&base, &json!({"env": {"EDITOR": "nano"}}), t(1));
    write_json(
        &p1,
        &json!({
            "theme": "dark",
            "env": {
                "MY_TOKEN": "p1-secret",
                "ANTHROPIC_BASE_URL": "https://p1.example",
                "EDITOR": "vim",
            }
        }),
        t(10),
    );
    write_json(&p2, &json!({"env": {"EDITOR": "nano"}}), t(5));

    sync();

    for (label, path) in [("base", &base), ("p2", &p2)] {
        let env = read_json(path)["env"].clone();
        assert!(
            env.get("MY_TOKEN").is_none(),
            "{label} received p1's custom [env] key: {env}"
        );
        assert!(
            env.get("ANTHROPIC_BASE_URL").is_none(),
            "{label} received p1's endpoint override: {env}"
        );
        assert_eq!(
            env["EDITOR"],
            json!("vim"),
            "{label} should still take the plain shared env var"
        );
    }
    // The winner keeps everything it had.
    assert_eq!(read_json(&p1)["env"]["MY_TOKEN"], json!("p1-secret"));
    assert_eq!(
        read_json(&p1)["env"]["ANTHROPIC_BASE_URL"],
        json!("https://p1.example")
    );
}

/// The protective direction: a target must KEEP the per-profile keys the winner
/// does not have. Every other fixture here gives both sides the same keys, which
/// leaves the two `retain`s in `jsonsync::merge_member` unreached — so this one
/// is deliberately asymmetric. An OAuth winner carries no `apiKeyHelper`, no
/// `model`, and no endpoint env; the api-key target carries all of them, and
/// losing any would send its next session's minted third-party key to
/// `api.anthropic.com`.
#[test]
fn an_api_key_target_keeps_its_routing_when_the_winner_has_none() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let oauth = runtime_path(home.home(), "p1");
    let apikey = runtime_path(home.home(), "p2");
    write_config(home.home(), "p1", "");
    write_config(home.home(), "p2", "[env]\nDEEPSEEK_TIER = \"paid\"\n");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    write_json(&oauth, &json!({"theme": "dark", "env": {}}), t(10));
    write_json(
        &apikey,
        &json!({
            "theme": "light",
            "apiKeyHelper": "clauth __api-key p2",
            "model": "deepseek-chat",
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
                "DEEPSEEK_TIER": "paid",
            }
        }),
        t(5),
    );

    sync();

    let after = read_json(&apikey);
    assert_eq!(
        after["apiKeyHelper"],
        json!("clauth __api-key p2"),
        "the winner has no apiKeyHelper; the target must not lose its own"
    );
    assert_eq!(after["model"], json!("deepseek-chat"));
    assert_eq!(
        after["env"]["ANTHROPIC_BASE_URL"],
        json!("https://api.deepseek.com/anthropic"),
        "losing this routes a DeepSeek key to api.anthropic.com"
    );
    assert_eq!(after["env"]["DEEPSEEK_TIER"], json!("paid"));
    assert_eq!(
        after["theme"],
        json!("dark"),
        "shared field still propagates"
    );
    // ...and none of it reached the members that never had it.
    for (label, path) in [("base", &base), ("p1", &oauth)] {
        let other = read_json(path);
        assert!(
            other.get("apiKeyHelper").is_none(),
            "{label} received p2's apiKeyHelper"
        );
        assert!(other.get("model").is_none(), "{label} received p2's model");
        assert_eq!(other["env"], json!({}), "{label} received p2's env");
    }
}

#[test]
fn api_key_helper_and_model_stay_exactly_where_they_were() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    // The home file carries the ACTIVE profile's own routing, written there by
    // `claude::apply_profile_to_claude_settings` on every switch.
    write_json(
        &base,
        &json!({"apiKeyHelper": "clauth __api-key active", "model": "opus", "env": {}}),
        t(1),
    );
    write_json(
        &p1,
        &json!({
            "apiKeyHelper": "clauth __api-key p1",
            "model": "sonnet",
            "theme": "dark",
            "env": {}
        }),
        t(10),
    );
    write_json(
        &p2,
        &json!({"apiKeyHelper": "clauth __api-key p2", "model": "haiku", "env": {}}),
        t(5),
    );

    let expected = [
        (&base, "clauth __api-key active", "opus"),
        (&p1, "clauth __api-key p1", "sonnet"),
        (&p2, "clauth __api-key p2", "haiku"),
    ];
    for (path, helper, model) in expected {
        let before = read_json(path);
        assert_eq!(before["apiKeyHelper"], json!(helper), "precondition");
        assert_eq!(before["model"], json!(model), "precondition");
    }

    sync();

    for (path, helper, model) in expected {
        let after = read_json(path);
        assert_eq!(
            after["apiKeyHelper"],
            json!(helper),
            "apiKeyHelper names the profile whose config.toml holds the raw key"
        );
        assert_eq!(after["model"], json!(model), "model routing is per-profile");
    }
    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "precondition: this sync did rewrite the base"
    );
    assert_eq!(read_json(&p2)["theme"], json!("dark"));
}

#[test]
fn unparseable_member_is_skipped_not_clobbered() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    write_json(&p1, &json!({}), t(2));
    // Newest by mtime, but caught mid-write → never read from nor written to.
    fs::write(&p1, b"{ \"theme\": \"dar").expect("write partial");
    set_mtime(&p1, t(20));
    write_json(&p2, &json!({"theme": "dark", "env": {}}), t(10));
    let before = fs::read(&p1).expect("read p1");

    sync();

    assert_eq!(fs::read(&p1).expect("read p1"), before);
    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "the newest PARSEABLE member is the winner"
    );
}

#[test]
fn isolated_runtime_neither_wins_nor_receives() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    let isolated = isolated_path(home.home(), "p1");
    write_json(
        &base,
        &json!({"theme": "light", "hooks": {"PreToolUse": ["x"]}, "env": {}}),
        t(1),
    );
    write_json(
        &p1,
        &json!({"theme": "dark", "hooks": {"PreToolUse": ["x"]}, "env": {}}),
        t(10),
    );
    write_json(
        &p2,
        &json!({"theme": "light", "hooks": {"PreToolUse": ["x"]}, "env": {}}),
        t(5),
    );
    // Built from an EMPTY base, and newest of all — a member would win and wipe
    // the operator's hooks everywhere.
    write_json(&isolated, &json!({"env": {}}), t(99));
    let before = fs::read(&isolated).expect("read isolated");

    sync();

    for (label, path) in [("base", &base), ("p2", &p2)] {
        let after = read_json(path);
        assert_eq!(
            after["hooks"],
            json!({"PreToolUse": ["x"]}),
            "{label} lost the operator's hooks to the empty-base isolated copy"
        );
        assert_eq!(after["theme"], json!("dark"), "{label} took p1's change");
    }
    assert_eq!(
        fs::read(&isolated).expect("read isolated"),
        before,
        "an isolated copy must not receive the shared fields either"
    );
    assert!(
        !known_paths().expect("known paths").contains(&isolated),
        "an isolated runtime's settings.json must not be a sync member"
    );
}

#[test]
fn shared_env_key_dropped_by_the_winner_is_removed_from_targets() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    write_json(
        &base,
        &json!({"env": {"EDITOR": "nano", "PAGER": "less"}}),
        t(1),
    );
    write_json(&p1, &json!({"env": {"EDITOR": "vim"}}), t(10));

    sync();

    let env = read_json(&base)["env"].clone();
    assert_eq!(env["EDITOR"], json!("vim"));
    assert!(
        env.get("PAGER").is_none(),
        "a shared env key the winner dropped must be removed: {env}"
    );
}

/// The engine's mode split, keyed on THIS reconciler's operator file. A runtime
/// copy carries an api-key profile's `apiKeyHelper` plus its endpoint env keys,
/// so it must land 0o600; `~/.claude/settings.json` lands at Claude Code's own
/// 0o644, the same posture `apply_profile_to_claude_settings` leaves it in. The
/// rename swaps the inode either way, so neither branch preserves an existing
/// mode — the point is which mode clauth imposes.
#[cfg(unix)]
#[test]
fn runtime_copies_are_owner_only_and_the_base_keeps_its_posture() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    write_json(&p1, &json!({"theme": "dark", "env": {}}), t(10));
    write_json(&p2, &json!({"theme": "light", "env": {}}), t(5));
    for path in [&base, &p2] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("chmod");
    }

    sync();

    let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(
        read_json(&p2)["theme"],
        json!("dark"),
        "precondition: this sync rewrote the runtime copy"
    );
    assert_eq!(mode(&p2), 0o600, "got {:#o}", mode(&p2));
    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "precondition: this sync rewrote the base"
    );
    assert_eq!(
        mode(&base),
        0o644,
        "~/.claude/settings.json is Claude Code's own file; do not restyle it"
    );
}

#[test]
fn every_managed_env_key_is_per_profile() {
    let custom_env = BTreeSet::new();
    for key in MANAGED_ENV_KEYS {
        assert_eq!(
            key_role(KeyPath::Nested(key), &custom_env),
            KeyRule::PerProfile,
            "{key} routes or authenticates one account and must never propagate"
        );
    }
    assert_eq!(
        key_role(KeyPath::Nested("EDITOR"), &custom_env),
        KeyRule::Shared
    );
}

/// Every credential-minting command and login-scoping key in Claude Code's own
/// settings schema, verified against the 2.1.215 binary. Propagating any of them
/// hands a sibling account the command that prints another account's secret, or
/// pins its login to the wrong org.
#[test]
fn every_credential_minting_top_level_key_is_per_profile() {
    let custom_env = BTreeSet::new();
    for key in [
        "apiKeyHelper",
        "proxyAuthHelper",
        "awsCredentialExport",
        "awsAuthRefresh",
        "gcpAuthRefresh",
        "otelHeadersHelper",
        "forceLoginMethod",
        "forceLoginGatewayUrl",
        "forceLoginOrgUUID",
        "model",
    ] {
        assert_eq!(
            key_role(KeyPath::Top(key), &custom_env),
            KeyRule::PerProfile,
            "{key} names one account's credential source or login scope"
        );
    }
    // Org-scoped neighbours in the same schema block, deliberately shared.
    for key in ["processWrapper", "policyHelper", "hooks", "statusLine"] {
        assert_eq!(
            key_role(KeyPath::Top(key), &custom_env),
            KeyRule::Shared,
            "{key} is operator/org preference, not account-scoped"
        );
    }
}

/// A winner whose `env` is not an object (hand-edit, or a truncation that still
/// parsed) must not erase the target's per-profile env. Copying it wholesale
/// would strip `ANTHROPIC_BASE_URL` from an api-key profile mid-session.
#[test]
fn a_non_object_env_on_the_winner_leaves_targets_alone() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    write_json(
        &base,
        &json!({"env": {"ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic"}}),
        t(1),
    );
    write_json(&p1, &json!({"theme": "dark", "env": null}), t(10));

    sync();

    let after = read_json(&base);
    assert_eq!(
        after["env"]["ANTHROPIC_BASE_URL"],
        json!("https://api.deepseek.com/anthropic"),
        "a null env on the winner must not wipe the target's endpoint"
    );
    assert_eq!(
        after["theme"],
        json!("dark"),
        "other shared keys still sync"
    );
}

#[test]
fn custom_env_keys_are_unioned_across_every_profile() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env]\nA_KEY = \"1\"\n");
    write_config(
        home.home(),
        "p2",
        "base_url = \"https://x\"\n[env]\nB_KEY = \"2\"\n",
    );
    write_config(home.home(), "p3", "");

    let keys = per_profile_env_keys().expect("every config.toml parses");

    assert_eq!(
        keys,
        BTreeSet::from(["A_KEY".to_string(), "B_KEY".to_string()])
    );
}

#[test]
fn a_config_that_does_not_parse_aborts_the_tick() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    write_config(home.home(), "p1", "[env]\nA_KEY = \"1\"\n");
    write_config(home.home(), "p2", "[env\nB_KEY = ");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    write_json(&p1, &json!({"theme": "dark", "env": {}}), t(10));

    assert!(
        per_profile_env_keys().is_none(),
        "an unknown per-profile env set must fail closed, not treat the keys as shared"
    );
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);

    assert!(
        !sync_members(&known_paths().expect("known paths")).expect("sync"),
        "the merge must not run while a profile's [env] set is unknown"
    );
    assert_eq!(
        read_json(&base)["theme"],
        json!("light"),
        "a paused tick must not merge anything"
    );
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
}

/// The pause is reported, but once — the watchdog retries at ~10 Hz, so an
/// unlatched log would flood. The latch clears on the next clean read.
#[test]
fn the_pause_warning_latches_and_clears_on_recovery() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env\nbroken = ");
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);

    assert!(per_profile_env_keys().is_none());
    assert!(
        ENV_KEYS_WARNED.load(Ordering::Relaxed),
        "the first failure must be reported"
    );
    assert!(per_profile_env_keys().is_none());
    assert!(ENV_KEYS_WARNED.load(Ordering::Relaxed), "still latched");

    write_config(home.home(), "p1", "[env]\nA_KEY = \"1\"\n");
    assert!(per_profile_env_keys().is_some());
    assert!(
        !ENV_KEYS_WARNED.load(Ordering::Relaxed),
        "a clean read must clear the latch so a recurrence is reported again"
    );
}

/// Harness gate (fork): a codex profile is invisible to settings sync — its dir
/// contributes no runtime/settings.json target, and its `config.toml` `[env]`
/// keys never join the per-profile custom set that classifies claude keys.
#[test]
fn codex_profiles_are_invisible_to_settings_sync() {
    let home = HomeSandbox::new();
    write_config(home.home(), "claude-p", "env = { MY_KEY = \"x\" }\n");
    write_config(
        home.home(),
        "cdx",
        "harness = \"codex\"\nenv = { CODEX_ONLY = \"y\" }\n",
    );

    let paths = known_paths().expect("known paths");
    assert!(
        paths.contains(&runtime_path(home.home(), "claude-p")),
        "claude member stays a sync target"
    );
    assert!(
        !paths.contains(&runtime_path(home.home(), "cdx")),
        "codex profile dir must not become a sync target"
    );

    let keys = per_profile_env_keys().expect("env keys read");
    assert!(keys.contains("MY_KEY"), "claude env key registers");
    assert!(
        !keys.contains("CODEX_ONLY"),
        "codex config.toml env keys never join the managed set"
    );
}
