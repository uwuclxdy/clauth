use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// A live session's own copy, `runtime-<sid>/settings.json`.
fn session_runtime_path(home: &Path, profile: &str, sid: &str) -> PathBuf {
    home.join(".clauth/profiles")
        .join(profile)
        .join(format!("runtime-{sid}"))
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

/// `advisorModel` is a model choice written per account (`[models] advisor`), so
/// a sibling's value must never reach another member: not the base, not a
/// runtime copy, and not the other way round.
#[test]
fn advisor_model_stays_exactly_where_it_was() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_json(&base, &json!({"advisorModel": "opus", "env": {}}), t(1));
    write_json(
        &p1,
        &json!({"advisorModel": "sonnet", "theme": "dark", "env": {}}),
        t(10),
    );
    // p2 has no advisor row, so its runtime copy carries no `advisorModel`.
    write_json(&p2, &json!({"env": {}}), t(5));

    sync();

    assert_eq!(read_json(&base)["advisorModel"], json!("opus"));
    assert_eq!(read_json(&p1)["advisorModel"], json!("sonnet"));
    assert!(
        read_json(&p2).get("advisorModel").is_none(),
        "p2 received a sibling's advisor, turning its advisor on"
    );
    assert_eq!(
        read_json(&p2)["theme"],
        json!("dark"),
        "precondition: this sync did rewrite p2"
    );
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

/// Regression for a break that compiles clean: every session now runs out of its
/// own `runtime-<sid>/`, so a discovery hardcoded to `<profile>/runtime` finds
/// nothing and `settings.json` reconciliation silently dies for every live
/// session. The isolated exclusion has to survive the widening.
#[test]
fn known_paths_reach_per_session_copies_and_still_exclude_isolated() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let legacy = runtime_path(home.home(), "p1");
    let session = session_runtime_path(home.home(), "p1", "4242-0");
    let sibling = session_runtime_path(home.home(), "p2", "4242-1");
    let isolated = home
        .home()
        .join(".clauth/profiles/p1")
        .join("runtime-isolated-4242-2")
        .join("settings.json");
    for path in [&base, &legacy, &session, &sibling, &isolated] {
        write_json(path, &json!({"env": {}}), t(1));
    }

    let paths = known_paths().expect("known paths");

    assert!(
        paths.contains(&base),
        "the operator base is always a member"
    );
    assert!(
        paths.contains(&session),
        "a live session's own runtime-<sid> copy must be a sync member"
    );
    assert!(
        paths.contains(&sibling),
        "another profile's session copy must be a member too"
    );
    assert!(
        paths.contains(&legacy),
        "a legacy unsuffixed runtime/ copy must stay a member"
    );
    assert!(
        !paths.contains(&isolated),
        "an isolated per-session copy must not be a sync member"
    );
    assert_eq!(paths.len(), 4, "no member beyond those four: {paths:#?}");
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
/// so it must land 0o600; `~/.claude/settings.json` lands at the writer's
/// umask-moded mode (0o644 at the default umask), the posture
/// `apply_profile_to_claude_settings` leaves it in. The
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
    let control = home.home().join("mode-control.json");
    crate::profile::atomic_write(&control, b"{}").expect("write mode control");
    assert_eq!(
        mode(&base),
        mode(&control),
        "the base lands at the writer's umask-moded mode, not the seed's (got {:#o}, control {:#o})",
        mode(&base),
        mode(&control),
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
        "advisorModel",
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

/// The union walks the same DIRECTORY listing the member set is derived from
/// — an off-roster drift dir keeps contributing (its runtime can still be a
/// sync member, so dropping its keys from the union would leak them as
/// shared) — and codex membership is the one exclusion: a codex `[env]` must
/// not join the claude union, and a codex config.toml that does not parse
/// must not pause a claude-only subsystem.
#[test]
fn a_codex_profiles_config_is_never_read_here() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env]\nA_KEY = \"1\"\n");

    let clauth = home.home().join(".clauth");
    crate::testutil::write_codex_roster(&["cx"]);
    let cx = clauth.join("profiles").join("cx");
    fs::create_dir_all(&cx).expect("mkdir codex profile");
    fs::write(cx.join("config.toml"), "[env\nBROKEN = ").expect("write broken codex config");

    // NOT on either roster: still a union contributor, because the member set
    // is directory-derived and would still merge this dir's runtime.
    write_config(home.home(), "drift", "[env]\nDRIFT_KEY = \"1\"\n");

    let keys = per_profile_env_keys()
        .expect("a codex config — parseable or not — must not pause the claude sync");
    assert_eq!(
        keys,
        BTreeSet::from(["A_KEY".to_string(), "DRIFT_KEY".to_string()])
    );
}

/// Uniqueness is enforced at creation, never against hand-edited state, so a
/// name BOTH files claim is reachable — and every other resolution site is
/// pinned claude-first. Skipping on the codex roster alone inverted it here,
/// which silently dropped the claude profile whose `[env]` leak this union
/// exists to catch.
#[test]
fn a_dual_claimed_name_stays_claude_first_here() {
    let home = HomeSandbox::new();
    write_config(home.home(), "shared", "[env]\nSHARED_KEY = \"1\"\n");

    let clauth = home.home().join(".clauth");
    fs::write(clauth.join("profiles.toml"), "profiles = [\"shared\"]\n")
        .expect("write claude roster");
    crate::testutil::write_codex_roster(&["shared"]);

    let keys = per_profile_env_keys().expect("every config.toml parses");
    assert!(
        keys.contains("SHARED_KEY"),
        "a dual-claimed name resolves claude-first, so its env still joins the union: {keys:?}"
    );
}

/// An unreadable codex roster is a codex file, and the codex skip exists so a
/// codex file cannot stall this claude subsystem: the codex arm reads as
/// absent, the walk proceeds, and the claude keys still arrive. Every other
/// arm keeps failing closed.
#[test]
fn an_unreadable_codex_roster_reads_as_no_codex_arm_and_never_pauses() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env]\nFOO = \"1\"\n");
    let clauth = home.home().join(".clauth");
    fs::write(clauth.join("codex-profiles.toml"), "profiles = [\n").expect("write broken roster");
    CODEX_ROSTER_WARNED.store(false, Ordering::Relaxed);

    assert_eq!(
        per_profile_env_keys().expect("the roster is not a claude file: never a pause"),
        BTreeSet::from(["FOO".to_string()])
    );
    assert!(
        CODEX_ROSTER_WARNED.load(Ordering::Relaxed),
        "the roster failure is reported once"
    );
    assert!(
        sync_members(&known_paths().expect("known paths")).expect("sync"),
        "the merge runs"
    );

    // A valid roster naming a codex dir whose own config.toml does not parse:
    // the dir is skipped by membership, the claude keys still arrive.
    crate::testutil::write_codex_roster(&["cx"]);
    let cx = clauth.join("profiles").join("cx");
    fs::create_dir_all(&cx).expect("mkdir codex profile");
    fs::write(cx.join("config.toml"), "[env\nBROKEN = ").expect("write broken codex config");
    assert_eq!(
        per_profile_env_keys().expect("a codex config is never read here"),
        BTreeSet::from(["FOO".to_string()])
    );
    assert!(
        !CODEX_ROSTER_WARNED.load(Ordering::Relaxed),
        "a clean roster read clears the latch so a recurrence is reported again"
    );

    // A claude config.toml that does not parse still pauses.
    write_config(home.home(), "p2", "[env\nBROKEN = ");
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
    assert!(
        per_profile_env_keys().is_none(),
        "the claude arm keeps failing closed"
    );
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
}

/// A torn codex roster names the position alone in the roster line, never the
/// api key in the parser's message — and the path only once, the line's own
/// copy, not re-quoted from the parse error.
#[test]
fn a_torn_codex_roster_names_the_position_and_never_the_secret() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env]\nFOO = \"1\"\n");
    let clauth = home.home().join(".clauth");
    // `api_key = "sk-c2probe-NOTAKEY` is an unterminated string: the span sits
    // at end-of-input, so the position is line 1, column 30.
    fs::write(
        clauth.join("codex-profiles.toml"),
        "api_key = \"sk-c2probe-NOTAKEY",
    )
    .expect("write torn roster");
    CODEX_ROSTER_WARNED.store(false, Ordering::Relaxed);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    assert_eq!(
        per_profile_env_keys().expect("the roster is not a claude file: never a pause"),
        BTreeSet::from(["FOO".to_string()])
    );
    drop(capture);

    let path = clauth.join("codex-profiles.toml");
    let expected = format!(
        "clauth: {} did not yield the codex roster (line 1, column 30); settings.json sync \
         reads every profile dir as claude until it reads cleanly",
        path.display()
    );
    let snapshot = lines.snapshot();
    assert!(
        snapshot.iter().any(|line| line == &expected),
        "the roster line names the position and the path once: expected {expected:?} in {snapshot:?}"
    );
    assert!(
        snapshot
            .iter()
            .all(|line| !line.contains("sk-c2probe-NOTAKEY")),
        "the api key must never reach the log: {snapshot:?}"
    );
}

/// A directory at the roster path is a read failure, not a parse failure: the
/// roster line keeps the io root cause in the parens, and the path appears
/// once. The expected parenthesised part is built from the same
/// `std::io::Error` the code sees (read-to-string over a directory), never a
/// hard-coded OS message, so it is portable across linux and windows.
#[test]
fn a_directory_at_the_roster_path_names_the_io_failure_once() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env]\nFOO = \"1\"\n");
    let clauth = home.home().join(".clauth");
    fs::create_dir_all(clauth.join("codex-profiles.toml")).expect("mkdir at the roster path");
    CODEX_ROSTER_WARNED.store(false, Ordering::Relaxed);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    assert_eq!(
        per_profile_env_keys().expect("the roster is not a claude file: never a pause"),
        BTreeSet::from(["FOO".to_string()])
    );
    drop(capture);

    let roster = clauth.join("codex-profiles.toml");
    let io = std::fs::read_to_string(&roster).expect_err("a directory cannot be read as text");
    let expected = format!(
        "clauth: {} did not yield the codex roster ({}); settings.json sync \
         reads every profile dir as claude until it reads cleanly",
        roster.display(),
        io
    );
    let snapshot = lines.snapshot();
    assert!(
        snapshot.iter().any(|line| line == &expected),
        "the io roster line names the io cause and the path once: expected {expected:?} in {snapshot:?}"
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

/// A torn `[env]` value would otherwise quote the api key in the pause
/// warning; the line names the position alone.
#[test]
fn a_torn_config_toml_pause_warning_names_the_position_and_never_the_secret() {
    let home = HomeSandbox::new();
    write_config(home.home(), "p1", "[env]\napi_key = \"sk-c2probe-NOTAKEY");
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    assert!(
        per_profile_env_keys().is_none(),
        "the torn env pauses the tick"
    );
    drop(capture);

    let path = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("config.toml");
    let expected = format!(
        "clauth: settings.json sync paused: {} is not valid TOML (line 2, column 30). \
         The affected profiles' custom [env] keys are unknown, so no settings are synced \
         until it reads cleanly",
        path.display()
    );
    let snapshot = lines.snapshot();
    assert!(
        snapshot.iter().any(|line| line == &expected),
        "the pause line names the position: expected {expected:?} in {snapshot:?}"
    );
    assert!(
        snapshot
            .iter()
            .all(|line| !line.contains("sk-c2probe-NOTAKEY")),
        "the api key must never reach the log: {snapshot:?}"
    );
}

/// The same mtime tie on the shared writer: a Claude Code write to the base
/// `settings.json` landing within one timestamp tick of clauth's copy write ties
/// the copy on mtime, and the tie must never go to clauth's own echo. The hook
/// stamps the base with the copy's mtime, so the tie holds on any filesystem.
#[test]
fn a_claude_code_write_tying_a_copy_write_on_mtime_wins_the_next_pass() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(10));
    write_json(&p1, &json!({"theme": "dark", "env": {}}), t(5));

    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::jsonsync::set_post_write_hook(Some(Box::new({
        let base = base.clone();
        let p1 = p1.clone();
        let fired = std::sync::Arc::clone(&fired);
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::Relaxed) != 0 {
                return;
            }
            let copy_mtime = fs::metadata(&p1)
                .expect("copy metadata")
                .modified()
                .expect("copy mtime");
            write_json(
                &base,
                &json!({"theme": "solarized", "ccNewKey": true, "env": {}}),
                copy_mtime,
            );
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_post_write_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    sync();
    crate::jsonsync::set_post_write_hook(None);

    assert_eq!(
        fired.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "precondition: clauth wrote the copy exactly once"
    );
    assert_eq!(
        fs::metadata(&base)
            .expect("meta")
            .modified()
            .expect("mtime"),
        fs::metadata(&p1).expect("meta").modified().expect("mtime"),
        "precondition: the base's write and the copy tie on mtime"
    );

    sync();
    assert_eq!(
        read_json(&base),
        json!({"theme": "solarized", "ccNewKey": true, "env": {}}),
        "the Claude Code write survives the tie"
    );
    assert_eq!(
        read_json(&p1),
        json!({"theme": "solarized", "ccNewKey": true, "env": {}}),
        "the copy converges on the Claude Code write"
    );
}

/// The settings written-copy record is owner-only and holds no member
/// bytes — only path, mtime (secs + nanos), size, and digest.
#[cfg(unix)]
#[test]
fn the_settings_record_is_owner_only_and_holds_no_member_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    write_json(&p1, &json!({"theme": "dark", "env": {}}), t(10));

    sync();

    let record = home.home().join(".clauth/.settings-sync-written.json");
    let meta = fs::metadata(&record).expect("the record exists");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "the settings record is owner-only"
    );

    let text = String::from_utf8(fs::read(&record).expect("read")).expect("utf-8");
    assert!(
        !text.contains("theme"),
        "the settings record holds no member bytes"
    );

    let entries: Vec<Value> = serde_json::from_str(&text).expect("record parses");
    assert_eq!(entries.len(), 1, "one copy was written (the base)");
    assert_eq!(
        entries[0]["path"],
        json!(base.to_string_lossy().into_owned())
    );
}
