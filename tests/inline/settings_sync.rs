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
    home.join(".clauth")
        .join("profiles")
        .join(profile)
        .join("runtime")
        .join("settings.json")
}

/// A live session's own copy, `runtime-<sid>/settings.json`.
fn session_runtime_path(home: &Path, profile: &str, sid: &str) -> PathBuf {
    home.join(".clauth")
        .join("profiles")
        .join(profile)
        .join(format!("runtime-{sid}"))
        .join("settings.json")
}

fn isolated_path(home: &Path, profile: &str) -> PathBuf {
    home.join(".clauth")
        .join("profiles")
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
    assert!(
        sync_members().expect("sync").merged,
        "the merge must have run; a paused tick means a config.toml did not parse"
    );
}

fn mtime_parts(when: SystemTime) -> (i64, u32) {
    let d = when
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("post-epoch");
    (d.as_secs() as i64, d.subsec_nanos())
}

/// A live `clauth start` session's flock-held marker on the sessions dir
/// `runtime::paired_sessions_name` pairs with `runtime_dir` (`runtime-<sid>` ↔
/// `sessions-<sid>`). Keep the returned handle alive for as long as the copy
/// should read as a live session's.
fn arm_runtime_marker(runtime_dir: &Path) -> std::fs::File {
    let name = runtime_dir
        .file_name()
        .expect("runtime dir name")
        .to_string_lossy()
        .replacen("runtime", "sessions", 1);
    let sessions = runtime_dir.with_file_name(name);
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("test-pid")).expect("open pid");
    pid.lock().expect("lock pid");
    pid
}

/// Write `path` as a copy clauth wrote: the production write-and-record path,
/// stamped at `when`, so the reconciler reads it as a recorded copy carrying
/// that write.
fn write_recorded_json(home: &Path, path: &Path, value: &Value, when: SystemTime) {
    let bytes = serde_json::to_vec_pretty(value).expect("serialize");
    crate::jsonsync::write_copy_and_record(
        Some(&home.join(".clauth").join(SETTINGS_SYNC_WRITTEN_FILENAME)),
        path,
        &bytes,
        mtime_parts(when),
        true,
        Some(when),
    )
    .expect("write recorded copy");
}

/// The production shape of one runtime `settings.json`: clauth wrote and
/// recorded it, and a live session holds the marker on its paired dir. Keep the
/// returned handle alive for the copy to stay a member.
fn live_recorded_json(home: &Path, path: &Path, value: &Value, when: SystemTime) -> std::fs::File {
    let marker = arm_runtime_marker(path.parent().expect("runtime dir"));
    write_recorded_json(home, path, value, when);
    marker
}

#[test]
fn shared_field_reaches_the_base_and_every_sibling_runtime() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
    let _live_p2 = live_recorded_json(
        home.home(),
        &p2,
        &json!({"theme": "light", "env": {}}),
        t(5),
    );

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
    let _live_p1 = live_recorded_json(
        home.home(),
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
    let _live_p2 = live_recorded_json(home.home(), &p2, &json!({"env": {"EDITOR": "nano"}}), t(5));

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

/// A key the active account covers stays per-profile while its cover is live,
/// even after a hand edit drops it from that account's `config.toml` before any
/// apply: `~/.clauth/base-env.json` still records the cover, so the account's
/// live value never reaches a sibling runtime.
#[test]
fn a_covered_key_dropped_from_its_config_by_hand_never_leaves_the_live_file() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p2 = runtime_path(home.home(), "p2");
    write_config(home.home(), "p1", "[env]\nOTHER = \"x\"\n");
    write_config(home.home(), "p2", "");
    let record = home.home().join(".clauth").join("base-env.json");
    fs::write(
        &record,
        r#"{"v":1,"keys":{"K":{"base":"user","wrote":"a913e7808d8b9993a294b7cec166ea558c8faaf8e32163ed36f83753330c4e0b"}}}"#,
    )
    .expect("write record");
    write_json(
        &base,
        &json!({"theme": "dark", "env": {"K": "p1-value", "OTHER": "x"}}),
        t(10),
    );
    let _live_p2 = live_recorded_json(home.home(), &p2, &json!({"env": {}}), t(5));

    sync();

    let env = read_json(&p2)["env"].clone();
    assert!(
        env.get("K").is_none(),
        "p2 received the active account's covered value: {env}"
    );
    assert_eq!(
        read_json(&p2)["theme"],
        json!("dark"),
        "control: the merge ran and carried a shared key"
    );
    assert_eq!(
        read_json(&base)["env"]["K"],
        json!("p1-value"),
        "the live file keeps its cover"
    );
}

/// A `~/.clauth/base-env.json` that does not parse pauses the merge like an
/// unparseable `config.toml`: the keys it covers are unknown, so nothing is
/// synced, and the one line it logs names the record, never what it holds.
#[test]
fn a_torn_base_env_record_pauses_the_merge_without_quoting_it() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p2 = runtime_path(home.home(), "p2");
    write_config(home.home(), "p1", "");
    write_config(home.home(), "p2", "");
    fs::write(
        home.home().join(".clauth").join("base-env.json"),
        r#"{"v":1,"keys":{"K":{"base":"sk-torn-secret""#,
    )
    .expect("write torn record");
    write_json(
        &base,
        &json!({"theme": "dark", "env": {"K": "p1-value"}}),
        t(10),
    );
    let _live_p2 = live_recorded_json(home.home(), &p2, &json!({"env": {}}), t(5));

    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let merged = sync_members().expect("sync").merged;
    drop(capture);
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);

    assert!(!merged, "the tick reports the pause");
    let synced = read_json(&p2);
    assert!(
        synced.get("theme").is_none() && synced["env"].get("K").is_none(),
        "the merge paused: {synced}"
    );
    let logged = lines.snapshot();
    assert_eq!(logged.len(), 1, "one pause line: {logged:?}");
    assert!(
        logged[0].contains("settings.json sync paused") && logged[0].contains("base-env.json"),
        "the line names the record: {logged:?}"
    );
    assert!(
        !logged[0].contains("sk-torn-secret"),
        "the line never quotes the record"
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
    let _live_oauth = live_recorded_json(
        home.home(),
        &oauth,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
    let _live_apikey = live_recorded_json(
        home.home(),
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
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({
            "apiKeyHelper": "clauth __api-key p1",
            "model": "sonnet",
            "theme": "dark",
            "env": {}
        }),
        t(10),
    );
    let _live_p2 = live_recorded_json(
        home.home(),
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

/// Effort is per account through its one carrier,
/// `env.CLAUDE_CODE_EFFORT_LEVEL`, an empty value included: it stays with the
/// member that holds it and never spreads to a sibling or the base.
/// `effortLevel` is Claude Code's own key, which clauth never writes, so it
/// syncs like any shared key.
#[test]
fn effort_stays_exactly_where_it_was() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let p1 = runtime_path(home.home(), "p1");
    let p2 = runtime_path(home.home(), "p2");
    write_json(
        &base,
        &json!({"effortLevel": "low", "env": {"CLAUDE_CODE_EFFORT_LEVEL": ""}}),
        t(1),
    );
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"effortLevel": "high", "theme": "dark", "env": {"CLAUDE_CODE_EFFORT_LEVEL": "max"}}),
        t(10),
    );
    let _live_p2 = live_recorded_json(home.home(), &p2, &json!({"env": {}}), t(5));

    sync();

    assert_eq!(
        read_json(&base)["env"]["CLAUDE_CODE_EFFORT_LEVEL"],
        json!(""),
        "the base kept its own empty override"
    );
    assert_eq!(
        read_json(&p1)["env"]["CLAUDE_CODE_EFFORT_LEVEL"],
        json!("max")
    );
    assert!(
        read_json(&p2)["env"]
            .get("CLAUDE_CODE_EFFORT_LEVEL")
            .is_none(),
        "p2 received a sibling's override"
    );
    assert_eq!(
        read_json(&p2)["effortLevel"],
        json!("high"),
        "effortLevel is shared: the newest member's value reaches p2"
    );
    assert_eq!(
        read_json(&p2)["theme"],
        json!("dark"),
        "precondition: this sync did rewrite p2"
    );
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
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"advisorModel": "sonnet", "theme": "dark", "env": {}}),
        t(10),
    );
    // p2 has no advisor row, so its runtime copy carries no `advisorModel`.
    let _live_p2 = live_recorded_json(home.home(), &p2, &json!({"env": {}}), t(5));

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
    let _live_p1 = arm_runtime_marker(p1.parent().expect("runtime dir"));
    // Newest by mtime, but caught mid-write → never read from nor written to.
    fs::write(&p1, b"{ \"theme\": \"dar").expect("write partial");
    set_mtime(&p1, t(20));
    let _live_p2 = live_recorded_json(
        home.home(),
        &p2,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
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
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("runtime-isolated-4242-2")
        .join("settings.json");
    for path in [&base, &legacy, &session, &sibling, &isolated] {
        write_json(path, &json!({"env": {}}), t(1));
    }
    // Each of the three shared copies is a live session's own tree; the
    // isolated copy has no live session armable through this walk at all.
    let _live_legacy = arm_runtime_marker(legacy.parent().expect("runtime dir"));
    let _live_session = arm_runtime_marker(session.parent().expect("runtime dir"));
    let _live_sibling = arm_runtime_marker(sibling.parent().expect("runtime dir"));

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
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"theme": "dark", "hooks": {"PreToolUse": ["x"]}, "env": {}}),
        t(10),
    );
    let _live_p2 = live_recorded_json(
        home.home(),
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
    let _live_p1 = live_recorded_json(home.home(), &p1, &json!({"env": {"EDITOR": "vim"}}), t(10));

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
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
    let _live_p2 = live_recorded_json(
        home.home(),
        &p2,
        &json!({"theme": "light", "env": {}}),
        t(5),
    );
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
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"theme": "dark", "env": null}),
        t(10),
    );

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
    assert!(sync_members().expect("sync").merged, "the merge runs");

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
        !sync_members().expect("sync").merged,
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
    let _live_p1 = arm_runtime_marker(p1.parent().expect("runtime dir"));

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
    let _live_p1 = live_recorded_json(
        home.home(),
        &p1,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );

    sync();

    let record = home
        .home()
        .join(".clauth")
        .join(SETTINGS_SYNC_WRITTEN_FILENAME);
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
    let mut recorded: Vec<String> = entries
        .iter()
        .map(|entry| {
            entry["path"]
                .as_str()
                .expect("every record entry names a path")
                .to_owned()
        })
        .collect();
    recorded.sort();
    let mut expected = vec![
        base.to_string_lossy().into_owned(),
        p1.to_string_lossy().into_owned(),
    ];
    expected.sort();
    assert_eq!(
        recorded, expected,
        "the record holds the base and the copy the pass wrote, nothing else"
    );
}

/// `is_recorded` asks what `can_win` asks: whether the record names the path,
/// whatever the bytes on disk now are.
#[test]
fn is_recorded_answers_by_path_even_after_claude_code_rewrote_the_copy() {
    let home = HomeSandbox::new();
    let record = home
        .home()
        .join(".clauth")
        .join(SETTINGS_SYNC_WRITTEN_FILENAME);
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    let other = session_runtime_path(home.home(), "p2", "4242-1");
    assert!(
        !crate::jsonsync::is_recorded(&record, &copy),
        "a missing record holds nothing"
    );

    write_recorded_json(home.home(), &copy, &json!({"env": {}}), t(1));
    write_json(&copy, &json!({"theme": "dark", "env": {}}), t(5));

    assert!(
        crate::jsonsync::is_recorded(&record, &copy),
        "a copy Claude Code rewrote keeps its entry"
    );
    assert!(!crate::jsonsync::is_recorded(&record, &other));

    fs::write(&record, b"{not json").expect("corrupt the record");
    assert!(
        !crate::jsonsync::is_recorded(&record, &copy),
        "a corrupt record holds nothing"
    );
}

#[test]
fn a_copy_no_live_session_holds_never_rewrites_the_base() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let rich = json!({"cleanupPeriodDays": 3650, "outputStyle": "x",
        "attribution": {"commit": ""}, "hooks": {"PreToolUse": []},
        "env": {"SHARED_VAR": "1"}, "theme": "dark"});
    write_json(&base_path(&h), &rich, t(10));
    // A stray Claude Code's first start in a runtime-named dir under a profile
    // clauth never made, and one beside a sessions dir whose marker no
    // process holds: both newer than the base, holding only the theme pick.
    write_json(
        &h.join(".clauth/profiles/zcode9/runtime-3532172-11/settings.json"),
        &json!({"theme": "dark"}),
        t(20),
    );
    fs::create_dir_all(h.join(".clauth/profiles/cld0/sessions-4242-0")).expect("mkdir");
    write_json(
        &h.join(".clauth/profiles/cld0/runtime-4242-0/settings.json"),
        &json!({"theme": "dark"}),
        t(21),
    );
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    assert_eq!(
        read_json(&base_path(&h)),
        rich,
        "a dead runtime copy took part in the sync and rewrote ~/.claude/settings.json"
    );
}

// ── the reconciler's three safety guards ────────────────────────────────────

/// The backups under `dir` named `<file>.<stamp>`, oldest first. An absent dir
/// has none.
fn backups_named(dir: &Path, file: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{file}.");
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    out.sort();
    out
}

/// A path as the refusal line renders it: the sandbox home shortened to `~`.
fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn settings_record(home: &Path) -> PathBuf {
    home.join(".clauth").join(SETTINGS_SYNC_WRITTEN_FILENAME)
}

/// Guard 2: a live runtime copy clauth never wrote still joins the pass (so its
/// per-profile state converges) but can never WIN it. A stray Claude Code's
/// first-start copy is exactly this shape: newest, and holding only the keys it
/// wrote, so winning would delete every shared key the base holds.
#[test]
fn a_live_copy_clauth_never_wrote_never_wins_the_base() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let rich = json!({"cleanupPeriodDays": 3650, "outputStyle": "x",
        "attribution": {"commit": ""}, "hooks": {"PreToolUse": []},
        "env": {"SHARED_VAR": "1"}, "theme": "light"});
    write_json(&base, &rich, t(1));
    let stray = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(&stray, &json!({"theme": "dark"}), t(20));
    let _live = arm_runtime_marker(stray.parent().expect("runtime dir"));

    sync();

    assert_eq!(
        read_json(&base),
        rich,
        "an unrecorded live copy won the pass and rewrote the base"
    );
}

/// Guard 2's positive control: a copy clauth WROTE, then carrying a newer
/// in-session edit, still propagates to the base — the sync's whole purpose.
#[test]
fn a_recorded_live_copy_carrying_a_newer_edit_still_reaches_the_base() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let live = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"theme": "light", "cleanupPeriodDays": 30, "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &live,
        &json!({"theme": "dark", "cleanupPeriodDays": 30, "env": {}}),
        t(10),
    );

    sync();

    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "a recorded live copy's newer shared edit must still reach the base"
    );
}

/// Guard 3: a recorded live winner lacking four of the base's shared top-level
/// keys is one over the bound, so the pass writes nothing, backs the base up
/// once, and says so with the exact line. The base holds a value clauth never
/// remembers, so no put-back reaches the winner before the guard does.
#[test]
fn a_winner_lacking_four_shared_keys_writes_nothing_and_backs_the_base_up() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 0, "outputStyle": "x",
            "hooks": {"PreToolUse": []}, "attribution": {"commit": ""}, "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(home.home(), &winner, &json!({"model": "opus"}), t(10));
    let record = settings_record(home.home());
    let backups_dir = home.home().join(".clauth").join("backups");

    let before_base = fs::read(&base).expect("read base");
    let before_winner = fs::read(&winner).expect("read winner");
    let before_record = fs::read(&record).expect("read record");

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(
        fs::read(&base).expect("read base"),
        before_base,
        "a refused pass must leave the base byte-identical"
    );
    assert_eq!(
        fs::read(&winner).expect("read winner"),
        before_winner,
        "a refused pass must leave every member byte-identical"
    );
    assert_eq!(
        fs::read(&record).expect("read record"),
        before_record,
        "a refused pass must not persist a record"
    );
    let backups = backups_named(&backups_dir, "settings.json");
    assert_eq!(backups.len(), 1, "exactly one backup: {backups:#?}");
    assert_eq!(
        fs::read(&backups[0]).expect("read backup"),
        before_base,
        "the backup holds the base's own bytes"
    );
    let expected = format!(
        "clauth: settings.json sync skipped: applying {} would delete 4 keys from {}; \
         saved a copy at {}",
        tilde(home.home(), &winner),
        tilde(home.home(), &base),
        tilde(home.home(), &backups[0]),
    );
    assert_eq!(
        lines.snapshot(),
        vec![expected],
        "the refusal line is pinned exactly"
    );

    {
        let _capture = lines.capture_here();
        sync();
    }
    assert_eq!(
        lines.snapshot().len(),
        1,
        "a second pass over the same winner must not raise a second line"
    );
    assert_eq!(
        backups_named(&backups_dir, "settings.json").len(),
        1,
        "a second pass over the same winner must not take a second backup"
    );
}

/// Guard 3's boundary: exactly three dropped shared keys is still a pass.
#[test]
fn a_winner_lacking_exactly_three_shared_keys_still_merges() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"outputStyle": "x", "hooks": {}, "attribution": {}, "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &winner,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );

    sync();

    assert_eq!(
        read_json(&base),
        json!({"theme": "dark", "env": {}}),
        "three dropped keys are under the bound, so the merge runs"
    );
    assert!(
        backups_named(
            &home.home().join(".clauth").join("backups"),
            "settings.json"
        )
        .is_empty(),
        "a pass under the bound takes no backup"
    );
}

/// Guard 3's retention: the newest ten backups of one file are kept, the oldest
/// goes on the eleventh refusal, and the other operator file's backups are
/// untouched. The base holds a value clauth never remembers, so no put-back
/// reaches the winner before the guard does.
#[test]
fn the_eleventh_refusal_keeps_ten_backups_and_spares_the_other_files() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 0, "outputStyle": "x",
            "hooks": {"PreToolUse": []}, "attribution": {"commit": ""}, "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(home.home(), &winner, &json!({"model": "opus"}), t(10));
    let backups_dir = home.home().join(".clauth").join("backups");
    fs::create_dir_all(&backups_dir).expect("mkdir backups");
    let other = backups_dir.join(".claude.json.2020-01-01T00-00-00Z");
    fs::write(&other, b"the other operator file's backup").expect("write other backup");

    let mut oldest = None;
    for round in 0..11u64 {
        // A fresh recorded write each round changes the winner's stat, which is
        // the refusal latch's key, so every round takes its own backup.
        write_recorded_json(
            home.home(),
            &winner,
            &json!({"model": "opus", "round": round}),
            t(10 + round),
        );
        sync();
        let backups = backups_named(&backups_dir, "settings.json");
        if round == 0 {
            oldest = backups.first().cloned();
        }
    }

    let backups = backups_named(&backups_dir, "settings.json");
    assert_eq!(backups.len(), 10, "ten backups are kept: {backups:#?}");
    let oldest = oldest.expect("the first refusal took a backup");
    assert!(
        !oldest.exists(),
        "the eleventh refusal must delete the oldest backup {oldest:?}"
    );
    assert!(
        other.exists(),
        "backups of the other operator file are never pruned here"
    );
}

/// The delete bound carries one protected key: a winner lacking
/// `cleanupPeriodDays` refuses even when that is the only shared key the pass
/// would delete, because its absence re-arms Claude Code's 30-day sweep. The
/// base holds a value clauth never remembers, so no put-back reaches the winner
/// before the guard does.
#[test]
fn a_winner_lacking_only_the_cleanup_retention_key_refuses_the_pass() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 0, "theme": "light", "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &winner,
        &json!({"theme": "light", "env": {}}),
        t(10),
    );
    let backups_dir = home.home().join(".clauth").join("backups");

    let before_base = fs::read(&base).expect("read base");

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(
        fs::read(&base).expect("read base"),
        before_base,
        "a pass deleting the retention key must leave the base byte-identical"
    );
    let backups = backups_named(&backups_dir, "settings.json");
    assert_eq!(backups.len(), 1, "exactly one backup: {backups:#?}");
    assert_eq!(
        fs::read(&backups[0]).expect("read backup"),
        before_base,
        "the backup holds the base's own bytes"
    );
    let expected = format!(
        "clauth: settings.json sync skipped: applying {} would delete 1 key from {}; \
         saved a copy at {}",
        tilde(home.home(), &winner),
        tilde(home.home(), &base),
        tilde(home.home(), &backups[0]),
    );
    assert_eq!(
        lines.snapshot(),
        vec![expected],
        "the refusal line is pinned exactly"
    );
}

/// The protection is the key's presence, not its value: a winner carrying
/// `cleanupPeriodDays` still merges, deleting one other shared key.
#[test]
fn a_winner_carrying_the_cleanup_retention_key_still_deletes_another_shared_key() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &winner,
        &json!({"cleanupPeriodDays": 30, "theme": "dark", "env": {}}),
        t(10),
    );

    sync();

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 30, "theme": "dark", "env": {}}),
        "a winner carrying the retention key still merges"
    );
}

// ── the guards pinned one at a time ──────────────────────────────────────────

/// Guard 1 alone: a DEAD runtime copy that is recorded (so guard 2 lets it win),
/// newest, and lacking only one non-protected shared key (so guard 3 lets the
/// pass run) is still excluded by membership, so the base keeps its keys.
#[test]
fn a_dead_recorded_copy_cannot_wipe_the_base_when_only_membership_can_stop_it() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        t(10),
    );
    // A dead tree under a profile clauth never made, and one beside a sessions
    // dir no process holds: both recorded and newest, each keeping the
    // protected retention key and lacking only one shared key.
    let ghost = h
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("runtime-3532172-11")
        .join("settings.json");
    write_recorded_json(
        &h,
        &ghost,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(20),
    );
    let stale = h
        .join(".clauth")
        .join("profiles")
        .join("cld0")
        .join("runtime-4242-0")
        .join("settings.json");
    fs::create_dir_all(
        h.join(".clauth")
            .join("profiles")
            .join("cld0")
            .join("sessions-4242-0"),
    )
    .expect("mkdir");
    write_recorded_json(
        &h,
        &stale,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(21),
    );
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        "a dead recorded copy took part in the sync and rewrote the base"
    );
}

/// Guard 2 alone: a LIVE runtime copy clauth never recorded, newest and lacking
/// only one non-protected shared key, is admitted (guard 1) and under the bound
/// (guard 3) — only the winner gate keeps the base intact.
#[test]
fn an_unrecorded_live_copy_cannot_win_when_only_the_winner_gate_can_stop_it() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        t(1),
    );
    let stray = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &stray,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(20),
    );
    let _live = arm_runtime_marker(stray.parent().expect("runtime dir"));

    sync();

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        "an unrecorded live copy won the pass and rewrote the base"
    );
}

/// The settings bound alone: a recorded live winner that KEEPS the protected
/// retention key but lacks four other shared keys refuses, so the >3 bound is
/// what stops the wipe, not the protected key.
#[test]
fn a_winner_keeping_the_retention_key_but_lacking_four_shared_keys_refuses() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "hooks": {},
            "attribution": {}, "theme": "dark", "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &winner,
        &json!({"cleanupPeriodDays": 3650, "env": {}}),
        t(10),
    );
    let backups_dir = home.home().join(".clauth").join("backups");

    let before_base = fs::read(&base).expect("read base");
    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(
        fs::read(&base).expect("read base"),
        before_base,
        "a refused pass must leave the base byte-identical"
    );
    let backups = backups_named(&backups_dir, "settings.json");
    assert_eq!(backups.len(), 1, "exactly one backup: {backups:#?}");
    assert_eq!(
        fs::read(&backups[0]).expect("read backup"),
        before_base,
        "the backup holds the base's own bytes"
    );
    let expected = format!(
        "clauth: settings.json sync skipped: applying {} would delete 4 keys from {}; \
         saved a copy at {}",
        tilde(home.home(), &winner),
        tilde(home.home(), &base),
        tilde(home.home(), &backups[0]),
    );
    assert_eq!(
        lines.snapshot(),
        vec![expected],
        "the refusal line is pinned exactly"
    );
}

/// A refusal whose backup cannot be written still refuses and writes nothing,
/// says once that it could not save a copy (naming the io error), and sets no
/// latch — so every later pass retries the backup, stays quiet while it keeps
/// failing, and logs the "saved a copy" line once the backup lands.
#[test]
fn a_failed_backup_still_refuses_and_the_next_pass_retries_it() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "hooks": {},
            "attribution": {}, "theme": "dark", "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &winner,
        &json!({"cleanupPeriodDays": 3650, "env": {}}),
        t(10),
    );
    let backups_dir = home.home().join(".clauth").join("backups");
    fs::create_dir_all(&backups_dir).expect("mkdir backups");

    let before_base = fs::read(&base).expect("read base");
    let lines = crate::logline::LogLines::new();
    let failed = {
        let _unwritable = crate::testutil::UnwritableDir::new(&backups_dir);
        // The io error the backup's own file create meets, as the platform
        // words it: taken off a create in the same refusing dir.
        let io_error = fs::File::create(backups_dir.join("probe"))
            .expect_err("the unwritable dir refuses a new file")
            .to_string();
        let failed = format!(
            "clauth: settings.json sync skipped: applying {} would delete 4 keys from {}; \
             could not save a copy in {}: {io_error}",
            tilde(home.home(), &winner),
            tilde(home.home(), &base),
            tilde(home.home(), &backups_dir),
        );
        {
            let _capture = lines.capture_here();
            sync();
        }
        assert_eq!(
            lines.snapshot(),
            vec![failed.clone()],
            "a refused pass whose backup failed says so once, naming the io error"
        );
        {
            let _capture = lines.capture_here();
            sync();
        }
        assert_eq!(
            lines.snapshot(),
            vec![failed.clone()],
            "a second pass over the same refused copy whose backup still fails stays quiet"
        );
        failed
    };
    assert_eq!(
        fs::read(&base).expect("read base"),
        before_base,
        "a refused pass whose backup failed must leave the base byte-identical"
    );
    assert!(
        backups_named(&backups_dir, "settings.json").is_empty(),
        "a refused pass whose backup failed must take no backup"
    );
    {
        let _capture = lines.capture_here();
        sync();
    }
    let backups = backups_named(&backups_dir, "settings.json");
    assert_eq!(
        backups.len(),
        1,
        "the retried pass backs up once: {backups:#?}"
    );
    assert_eq!(
        fs::read(&backups[0]).expect("read backup"),
        before_base,
        "the retried pass backs up the base's own bytes"
    );
    let expected = format!(
        "clauth: settings.json sync skipped: applying {} would delete 4 keys from {}; \
         saved a copy at {}",
        tilde(home.home(), &winner),
        tilde(home.home(), &base),
        tilde(home.home(), &backups[0]),
    );
    assert_eq!(
        lines.snapshot(),
        vec![failed, expected],
        "the retried pass logs the saved-copy line exactly once, after the failure line"
    );
}

/// The job half of membership: a shared tree with no marker is still a member
/// while a live background job NAMES it and its supervisor record reads live,
/// so its newer edit propagates. Deleting the job term must red this (the tree
/// then reads as dead and is excluded).
#[test]
fn a_job_named_tree_with_no_marker_joins_the_members_and_syncs() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let tree = h
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("runtime-bg");
    let copy = tree.join("settings.json");
    write_recorded_json(&h, &copy, &json!({"theme": "dark", "env": {}}), t(10));
    crate::testutil::write_claude_job(&h, "0add99aa", &tree);
    crate::testutil::write_live_supervisor(&tree);
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "a job-named tree with no marker must join the sync"
    );
}

/// An unreadable job record must EXCLUDE every dead tree, never admit it: the
/// job term reads an unreadable store as naming no tree — the reverse of the
/// GC, which spares every tree it cannot read — so a torn `state.json` leaves a
/// dead recorded copy out of the pass and the base byte-identical.
#[test]
fn an_unreadable_job_record_excludes_every_dead_tree() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        t(10),
    );
    // A dead recorded copy, newest, keeping the protected retention key and
    // lacking only one shared key: admitted, it would win and drop `outputStyle`.
    let ghost = h
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("runtime-3532172-11")
        .join("settings.json");
    write_recorded_json(
        &h,
        &ghost,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(20),
    );
    let job = h.join(".claude").join("jobs").join("0add99ed");
    fs::create_dir_all(&job).expect("mkdir job dir");
    fs::write(
        job.join("state.json"),
        br#"{"state":"working","providerEnv":{"CLAUDE_CON"#,
    )
    .expect("torn job record");
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        "an unreadable job record must exclude the dead tree, not admit it"
    );
}

/// An unreadable session marker must EXCLUDE the dead tree, never admit it: the
/// marker term reads a marker it cannot open as unknown, so a dead recorded copy
/// whose only marker is unreadable stays out of the pass and the base stays
/// byte-identical. Unix-only: mode 000 yields the open error portably there.
#[cfg(unix)]
#[test]
fn an_unreadable_marker_excludes_the_dead_tree() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        t(10),
    );
    // A dead recorded copy, newest, keeping the protected retention key and
    // lacking only one shared key: admitted, it would win and drop `outputStyle`.
    let ghost = h
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("runtime-3532172-11")
        .join("settings.json");
    write_recorded_json(
        &h,
        &ghost,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(20),
    );
    // A crashed session left its marker behind at mode 000: the probe's open
    // fails with EACCES, which the marker term reads as unknown, never live.
    let sessions = h
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("sessions-3532172-11");
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let marker = sessions.join("test-pid");
    fs::write(&marker, b"x").expect("write marker");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        "an unreadable marker must exclude the dead tree, not admit it"
    );
}

/// A readable but unlocked marker still reads as a dead session: the marker term
/// folds neither an open nor a lock success into liveness, so a dead recorded
/// copy whose marker nobody holds stays out of the pass and the base stays
/// byte-identical.
#[test]
fn a_readable_unlocked_marker_keeps_the_dead_tree_out() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        t(10),
    );
    let ghost = h
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("runtime-3532172-11")
        .join("settings.json");
    write_recorded_json(
        &h,
        &ghost,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(20),
    );
    let sessions = h
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("sessions-3532172-11");
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    fs::write(sessions.join("test-pid"), b"x").expect("write marker");
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "outputStyle": "x", "theme": "dark", "env": {}}),
        "a readable unlocked marker must exclude the dead tree, not admit it"
    );
}

/// The refusal latch is keyed per operator file: a `.claude.json` refusal must
/// not re-arm a standing settings refusal. Settings refuses once, then
/// `.claude.json` refuses once, then the same settings pass runs again: still
/// exactly one settings line and one settings backup in total.
#[test]
fn a_claude_json_refusal_does_not_rearm_a_standing_settings_refusal() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x",
            "hooks": {}, "attribution": {}, "theme": "dark", "env": {}}),
        t(1),
    );
    let s_winner = session_runtime_path(&h, "p1", "4242-0");
    let _live = live_recorded_json(
        &h,
        &s_winner,
        &json!({"cleanupPeriodDays": 3650, "env": {}}),
        t(10),
    );

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    // A `.claude.json` refusal on the same live tree: home holds four shared
    // keys and the recorded newest winner holds none of them.
    let c_home = h.join(".claude.json");
    write_json(
        &c_home,
        &json!({"numStartups": 1, "projects": {}, "userID": "u", "installMethod": "native"}),
        t(1),
    );
    let c_winner = h
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("runtime-4242-0")
        .join(".claude.json");
    let c_bytes = serde_json::to_vec_pretty(&json!({"firstStartTime": "x"})).expect("serialize");
    crate::jsonsync::write_copy_and_record(
        crate::claude_json::written_record_path().as_deref(),
        &c_winner,
        &c_bytes,
        mtime_parts(t(10)),
        true,
        Some(t(10)),
    )
    .expect("write recorded .claude.json winner");
    {
        let _capture = lines.capture_here();
        crate::claude_json::merge_paths(&[c_home.clone(), c_winner.clone()])
            .expect(".claude.json sync");
    }

    // The same settings pass again: the standing settings refusal must stay
    // latched, so it must not back up or log a second time.
    {
        let _capture = lines.capture_here();
        sync();
    }

    let snapshot = lines.snapshot();
    let settings_lines: Vec<&String> = snapshot
        .iter()
        .filter(|l| l.contains("settings.json sync skipped"))
        .collect();
    assert_eq!(
        settings_lines.len(),
        1,
        "exactly one settings line in total: {snapshot:#?}"
    );
    assert_eq!(
        backups_named(&h.join(".clauth").join("backups"), "settings.json").len(),
        1,
        "exactly one settings backup in total"
    );
    assert_eq!(
        backups_named(&h.join(".clauth").join("backups"), ".claude.json").len(),
        1,
        "the .claude.json refusal took its own one backup"
    );
}

/// The merge re-reads the member list inside the state lock, so a pass whose
/// fast-path list was read before a just-started session's job named its tree
/// still keeps that session's record entry (a start records its copy). The
/// pre-lock hook poses the start between the fast path's list and the lock;
/// moving the list read back outside the lock reds this by pruning the entry.
#[test]
fn a_pass_that_listed_members_before_a_session_started_keeps_its_record_entry() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let p1 = session_runtime_path(&h, "p1", "4242-0");
    let _live1 = live_recorded_json(&h, &p1, &json!({"theme": "dark", "env": {}}), t(10));
    // A session starting between the fast path's list and the lock: its copy is
    // recorded (as a start records it) and a job names its tree (so the re-read
    // inside the lock admits it without a marker).
    let p2 = session_runtime_path(&h, "p2", "4242-1");
    let tree2 = p2.parent().expect("runtime dir").to_path_buf();
    set_pre_lock_hook(Some(Box::new({
        let h = h.clone();
        let p2 = p2.clone();
        move || {
            write_recorded_json(&h, &p2, &json!({"theme": "dark", "env": {}}), t(20));
            crate::testutil::write_claude_job(&h, "0add99bb", &tree2);
            crate::testutil::write_live_supervisor(&tree2);
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_pre_lock_hook(None);
        }
    }
    let _hook_guard = HookGuard;
    *LAST_SYNCED.lock().expect("lock") = None;

    sync_once().expect("sync");

    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(settings_record(&h)).expect("read record"))
            .expect("record parses");
    let p2_str = p2.to_string_lossy();
    assert!(
        entries.iter().any(|e| e["path"].as_str() == Some(&*p2_str)),
        "the just-started session's record entry must survive the pass: {entries:#?}"
    );
}

/// A running session whose copy no record names (a start by an older clauth
/// left it unrecorded) but whose bytes already equal the merge is recorded on
/// the first pass that sees it, without a rewrite, so the session's later
/// in-session edit can win and reach the base.
#[test]
fn a_live_unrecorded_copy_matching_the_merge_is_recorded_and_its_edit_reaches_the_base() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let copy = session_runtime_path(&h, "p1", "4242-0");
    write_json(&copy, &json!({"theme": "light", "env": {}}), t(5));
    let _live = arm_runtime_marker(copy.parent().expect("runtime dir"));
    crate::jsonsync::reset_write_counts();

    sync();

    assert_eq!(
        crate::jsonsync::write_count(&copy),
        0,
        "a copy already equal to the merge is recorded without a rewrite"
    );
    // A pass that writes nothing leaves no record file at all on the old code:
    // read a missing record as empty so the assertion below is what reds.
    let entries: Vec<Value> = fs::read(settings_record(&h))
        .map(|bytes| serde_json::from_slice(&bytes).expect("record parses"))
        .unwrap_or_default();
    let copy_str = copy.to_string_lossy();
    assert!(
        entries
            .iter()
            .any(|e| e["path"].as_str() == Some(&*copy_str)),
        "the matching live copy must be recorded on first sight: {entries:#?}"
    );

    write_json(&copy, &json!({"theme": "dark", "env": {}}), t(20));
    sync();

    assert_eq!(
        read_json(&base),
        json!({"theme": "dark", "env": {}}),
        "the session's in-session edit must reach the base once its copy is recorded"
    );
}

/// A live session's tree the sync leaves out for a moment because its liveness
/// reads UNKNOWN (here: its held marker cannot be opened) keeps its record entry
/// through a pass that writes the other members; once it rejoins, its in-session
/// edit wins and reaches the base instead of being overwritten. Unix-only: mode
/// 000 is the portable way to make the marker probe's open fail.
#[cfg(unix)]
#[test]
fn a_live_tree_excluded_for_a_moment_keeps_its_record_entry_and_its_edit_wins() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let p1 = session_runtime_path(&h, "p1", "4242-0");
    let p2 = session_runtime_path(&h, "p2", "4242-1");
    let _live1 = live_recorded_json(&h, &p1, &json!({"theme": "light", "env": {}}), t(5));
    let _live2 = live_recorded_json(&h, &p2, &json!({"theme": "dark", "env": {}}), t(10));
    let marker = h
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("sessions-4242-0")
        .join("test-pid");

    // p1's marker stays held, but the probe cannot open it, so p1's liveness
    // reads unknown and p1 is left out while p2 wins and the pass rewrites
    // the base.
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    assert!(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&marker)
            .is_err(),
        "the mode-000 marker must refuse an open (a root run would read it live)"
    );
    sync();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).expect("chmod 600");
    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "the pass without p1 must still run and rewrite the base"
    );
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(settings_record(&h)).expect("read record"))
            .expect("record parses");
    let p1_str = p1.to_string_lossy();
    assert!(
        entries.iter().any(|e| e["path"].as_str() == Some(&*p1_str)),
        "a left-out tree that still exists keeps its record entry: {entries:#?}"
    );

    // p1's marker reads again: p1 rejoins carrying the session's own newer edit.
    write_json(&p1, &json!({"theme": "blue", "env": {}}), t(30));
    sync();

    assert_eq!(
        read_json(&base)["theme"],
        json!("blue"),
        "the rejoined session's edit must win, not be overwritten"
    );
}

/// The record-on-first-sight rule is for runtime copies only: a pass whose
/// winner is a live copy and whose base already equals the merge rewrites
/// nothing and records no entry for the base, which is not a clauth copy.
#[test]
fn a_base_already_equal_to_the_merge_gets_no_record_entry() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "dark", "env": {}}), t(1));
    let copy = session_runtime_path(&h, "p1", "4242-0");
    let _live = live_recorded_json(&h, &copy, &json!({"theme": "dark", "env": {}}), t(10));
    let before = fs::read(settings_record(&h)).expect("read record");

    sync();

    assert_eq!(
        fs::read(settings_record(&h)).expect("read record"),
        before,
        "a converged pass must leave the record as it was: no entry for the base"
    );
}

/// A recorded copy that already equals the merge keeps its own entry, carried
/// rank included: the first-sight rule records only a copy the record holds no
/// entry for, so a converged pass never re-ranks an echo at the winner's rank.
#[test]
fn a_converged_pass_keeps_a_recorded_copys_carried_rank() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "dark", "env": {}}), t(20));
    let copy = session_runtime_path(&h, "p1", "4242-0");
    let _live = live_recorded_json(&h, &copy, &json!({"theme": "dark", "env": {}}), t(10));
    let before = fs::read(settings_record(&h)).expect("read record");

    sync();

    assert_eq!(
        fs::read(settings_record(&h)).expect("read record"),
        before,
        "a converged pass must keep the recorded copy's entry and its carried rank"
    );
}

/// A tree whose session is known DEAD (its marker readable and unheld) loses its
/// record entry on the next pass that writes, even while its dir still exists:
/// an edit landing in it while dead (a stray Claude Code pointed at the tree)
/// must not win once a later session makes the tree live again.
#[test]
fn a_dead_tree_loses_its_record_entry_so_an_edit_made_while_dead_never_wins() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let p1 = session_runtime_path(&h, "p1", "4242-0");
    let p2 = session_runtime_path(&h, "p2", "4242-1");
    let live1 = live_recorded_json(&h, &p1, &json!({"theme": "light", "env": {}}), t(5));
    let _live2 = live_recorded_json(&h, &p2, &json!({"theme": "dark", "env": {}}), t(10));

    // p1's session ends: its marker stays on disk, readable and unheld.
    drop(live1);
    sync();
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(settings_record(&h)).expect("read record"))
            .expect("record parses");
    let p1_str = p1.to_string_lossy();
    assert!(
        !entries.iter().any(|e| e["path"].as_str() == Some(&*p1_str)),
        "a known-dead tree must lose its record entry: {entries:#?}"
    );

    // A stray edit lands in the dead tree, then a session makes it live again.
    write_json(&p1, &json!({"theme": "blue", "env": {}}), t(30));
    let _relive = arm_runtime_marker(p1.parent().expect("runtime dir"));
    sync();

    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "an edit made while the tree was dead must never win"
    );
}

/// The dead-tree prune does not wait for a pass that writes: when the only
/// session ends, the next pass has the base alone to merge and writes nothing,
/// yet it still drops the dead tree's entry, so a stray edit there never wins
/// once the tree is live again.
#[test]
fn the_last_sessions_death_drops_its_entry_on_a_pass_that_writes_nothing() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let p1 = session_runtime_path(&h, "p1", "4242-0");
    let live1 = live_recorded_json(&h, &p1, &json!({"theme": "light", "env": {}}), t(5));

    drop(live1);
    sync();
    let entries: Vec<Value> = fs::read(settings_record(&h))
        .map(|bytes| serde_json::from_slice(&bytes).expect("record parses"))
        .unwrap_or_default();
    let p1_str = p1.to_string_lossy();
    assert!(
        !entries.iter().any(|e| e["path"].as_str() == Some(&*p1_str)),
        "the last session's dead tree must lose its entry: {entries:#?}"
    );

    write_json(&p1, &json!({"theme": "blue", "env": {}}), t(30));
    let _relive = arm_runtime_marker(p1.parent().expect("runtime dir"));
    sync();

    assert_eq!(
        read_json(&base)["theme"],
        json!("light"),
        "an edit made while the only tree was dead must never win"
    );
}

/// Unknown liveness through a torn global job record (portable: no chmod): a
/// tree whose marker nobody holds while the job store cannot be read keeps its
/// record entry through a pass that writes the other members.
#[test]
fn a_tree_left_out_while_the_job_store_is_torn_keeps_its_record_entry() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let p1 = session_runtime_path(&h, "p1", "4242-0");
    let p2 = session_runtime_path(&h, "p2", "4242-1");
    let live1 = live_recorded_json(&h, &p1, &json!({"theme": "light", "env": {}}), t(5));
    let _live2 = live_recorded_json(&h, &p2, &json!({"theme": "dark", "env": {}}), t(10));
    drop(live1);
    let job = h.join(".claude").join("jobs").join("0add99ed");
    fs::create_dir_all(&job).expect("mkdir job dir");
    fs::write(
        job.join("state.json"),
        br#"{"state":"working","providerEnv":{"CLAUDE_CON"#,
    )
    .expect("torn job record");

    sync();

    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "the pass without p1 must still run and rewrite the base"
    );
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(settings_record(&h)).expect("read record"))
            .expect("record parses");
    let p1_str = p1.to_string_lossy();
    assert!(
        entries.iter().any(|e| e["path"].as_str() == Some(&*p1_str)),
        "a tree whose job store cannot be read reads unknown and keeps its entry: {entries:#?}"
    );
}

/// The same through a torn job record in the tree's OWN jobs store: that
/// store's unreadability is unknown for this tree, never dead.
#[test]
fn a_tree_whose_own_job_store_is_torn_keeps_its_record_entry() {
    let home = HomeSandbox::new();
    let h = home.home().to_path_buf();
    let base = base_path(&h);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(1));
    let p1 = session_runtime_path(&h, "p1", "4242-0");
    let p2 = session_runtime_path(&h, "p2", "4242-1");
    let live1 = live_recorded_json(&h, &p1, &json!({"theme": "light", "env": {}}), t(5));
    let _live2 = live_recorded_json(&h, &p2, &json!({"theme": "dark", "env": {}}), t(10));
    drop(live1);
    let job = p1
        .parent()
        .expect("runtime dir")
        .join("jobs")
        .join("0add99ee");
    fs::create_dir_all(&job).expect("mkdir job dir");
    fs::write(
        job.join("state.json"),
        br#"{"state":"working","providerEnv":{"CLAUDE_CON"#,
    )
    .expect("torn job record");

    sync();

    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "the pass without p1 must still run and rewrite the base"
    );
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(settings_record(&h)).expect("read record"))
            .expect("record parses");
    let p1_str = p1.to_string_lossy();
    assert!(
        entries.iter().any(|e| e["path"].as_str() == Some(&*p1_str)),
        "a tree whose own job store cannot be read reads unknown and keeps its entry: {entries:#?}"
    );
}

/// `shorten_home` renders the home prefix as `~` plus the platform separator,
/// never a hardcoded `/` (which would mix separators with a Windows `\`), and
/// leaves a path outside home to `Path::display`.
#[test]
fn shorten_home_renders_the_platform_separator_and_leaves_foreign_paths_alone() {
    let home = HomeSandbox::new();
    let under = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("runtime-4242-0")
        .join("settings.json");
    let rest = under.strip_prefix(home.home()).expect("under home");
    let expected = format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
    assert_eq!(
        crate::jsonsync::shorten_home(&under),
        expected,
        "a path under home renders ~ + MAIN_SEPARATOR + the rest"
    );
    let foreign = std::path::PathBuf::from("/elsewhere").join("x.json");
    assert_eq!(
        crate::jsonsync::shorten_home(&foreign),
        foreign.display().to_string(),
        "a path outside home renders through Path::display"
    );
}

// ── the remembered transcript retention ─────────────────────────────────────

/// The one line a put-back into the operator file logs.
fn put_back_line(days: u64) -> String {
    format!(
        "clauth: cleanupPeriodDays was gone from ~/.claude/settings.json, put {days} back; \
         open clauth to keep or remove it"
    )
}

fn retention() -> crate::retention::Retention {
    crate::retention::load().expect("load the retention record")
}

fn remembered(days: u64, question_open: bool) -> crate::retention::Retention {
    crate::retention::Retention {
        days: Some(days),
        question_open,
    }
}

/// Seed the remembered number the way production learns it: one pass over an
/// operator file that holds it.
fn remember_through_a_pass(home: &Path, days: u64) {
    write_json(
        &base_path(home),
        &json!({"cleanupPeriodDays": days, "theme": "light", "env": {}}),
        t(1),
    );
    sync();
    assert_eq!(
        retention(),
        remembered(days, false),
        "the pass remembers the number"
    );
}

#[test]
fn a_keyless_operator_file_gets_the_remembered_number_back_with_one_line_and_an_open_question() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "light", "env": {}}),
        t(5),
    );
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
        sync();
    }

    let expected = json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}});
    assert_eq!(
        read_json(&base),
        expected,
        "the operator file holds the number again"
    );
    assert_eq!(
        read_json(&copy),
        expected,
        "the live copy holds the number too"
    );
    assert_eq!(retention(), remembered(3650, true), "the question is open");
    assert_eq!(
        lines.snapshot(),
        vec![put_back_line(3650)],
        "one line for one loss, however many passes follow"
    );
}

/// The put-back must not make the operator file the newest member: a live
/// copy's newer in-session edit still wins the same pass.
#[test]
fn a_put_back_keeps_a_newer_in_session_edit() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        t(30),
    );

    sync();

    let expected = json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}});
    assert_eq!(
        read_json(&base),
        expected,
        "the in-session edit reaches the operator file"
    );
    assert_eq!(read_json(&copy), expected, "the copy keeps its edit");
}

#[test]
fn a_new_number_by_hand_replaces_the_remembered_one_with_no_question() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    remember_through_a_pass(home.home(), 3650);
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 365, "theme": "light", "env": {}}),
        t(20),
    );

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(retention(), remembered(365, false));
    assert_eq!(read_json(&base)["cleanupPeriodDays"], json!(365));
    assert!(lines.snapshot().is_empty(), "a changed number logs nothing");
}

#[test]
fn a_missing_operator_file_stays_missing_while_the_live_copy_gets_the_number() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    fs::remove_file(&base).expect("remove the operator file");
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "light", "env": {}}),
        t(10),
    );

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert!(!base.exists(), "clauth never creates the operator file");
    assert_eq!(read_json(&copy)["cleanupPeriodDays"], json!(3650));
    assert_eq!(
        retention(),
        remembered(3650, false),
        "no question without a put-back"
    );
    assert!(
        lines.snapshot().is_empty(),
        "nothing was put back into the operator file"
    );
}

#[test]
fn an_unparseable_operator_file_keeps_its_bytes_while_the_live_copy_gets_the_number() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let torn = b"{\"theme\": \"light\", \"env\": {";
    fs::write(&base, torn).expect("write a torn operator file");
    set_mtime(&base, t(20));
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "light", "env": {}}),
        t(10),
    );

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(
        fs::read(&base).expect("read base"),
        torn,
        "the torn file is left alone"
    );
    assert_eq!(read_json(&copy)["cleanupPeriodDays"], json!(3650));
    assert_eq!(retention(), remembered(3650, false));
    assert_eq!(lines.snapshot(), Vec::<String>::new());
}

#[test]
fn nothing_remembered_puts_nothing_back() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    let keyless = json!({"theme": "light", "env": {}});
    write_json(&base, &keyless, t(1));
    let _live = live_recorded_json(home.home(), &copy, &keyless, t(1));
    let before = fs::read(&base).expect("read base");

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(fs::read(&base).expect("read base"), before);
    assert!(read_json(&copy).get("cleanupPeriodDays").is_none());
    assert_eq!(retention(), crate::retention::Retention::default());
    assert_eq!(lines.snapshot(), Vec::<String>::new());
}

/// Claude Code's schema takes a positive integer only; anything else in the
/// operator file is never remembered, and a remembered number is kept.
#[test]
fn a_value_claude_code_rejects_is_never_remembered() {
    for rejected in [json!(0), json!(-7), json!(1.5), json!("3650"), json!(null)] {
        let home = HomeSandbox::new();
        let base = base_path(home.home());
        write_json(
            &base,
            &json!({"cleanupPeriodDays": rejected.clone(), "env": {}}),
            t(1),
        );
        sync();
        assert_eq!(
            retention(),
            crate::retention::Retention::default(),
            "{rejected} is not remembered"
        );

        remember_through_a_pass(home.home(), 3650);
        write_json(
            &base,
            &json!({"cleanupPeriodDays": rejected.clone(), "env": {}}),
            t(20),
        );
        sync();
        assert_eq!(
            retention(),
            remembered(3650, false),
            "{rejected} leaves the remembered number in place"
        );
        assert_eq!(
            read_json(&base)["cleanupPeriodDays"],
            rejected,
            "a key that is present is never overwritten"
        );
    }
}

#[test]
fn an_isolated_copy_never_gets_the_number() {
    let home = HomeSandbox::new();
    let isolated = isolated_path(home.home(), "p1");
    remember_through_a_pass(home.home(), 3650);
    write_json(&isolated, &json!({"env": {}}), t(10));

    sync();

    assert!(read_json(&isolated).get("cleanupPeriodDays").is_none());
}

#[test]
fn remove_forgets_the_number_and_deletes_the_key_everywhere_until_a_new_number_appears() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(5),
    );
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    sync();
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the loss opened the question"
    );

    crate::retention::remove().expect("remove");

    let keyless = json!({"theme": "light", "env": {}});
    assert_eq!(read_json(&base), keyless, "the operator file loses the key");
    assert_eq!(read_json(&copy), keyless, "the live copy loses the key");
    assert_eq!(retention(), crate::retention::Retention::default());

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }
    assert_eq!(read_json(&base), keyless, "a later pass puts nothing back");
    assert_eq!(read_json(&copy), keyless);
    assert_eq!(lines.snapshot(), Vec::<String>::new());

    write_json(
        &base,
        &json!({"cleanupPeriodDays": 30, "theme": "light", "env": {}}),
        t(40),
    );
    sync();
    assert_eq!(
        retention(),
        remembered(30, false),
        "a new number arms it again"
    );
}

/// The daemon's leg: with no live session there is no reconcile pass, and the
/// check still puts the number back.
#[test]
fn the_daemon_check_puts_the_number_back_with_no_live_session() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(1),
    );
    crate::retention::reconcile().expect("reconcile");
    assert_eq!(retention(), remembered(3650, false));
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        crate::retention::reconcile().expect("reconcile");
    }

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}})
    );
    assert_eq!(retention(), remembered(3650, true));
    assert_eq!(lines.snapshot(), vec![put_back_line(3650)]);
}

#[cfg(unix)]
#[test]
fn the_retention_record_is_owner_only_under_clauth_home() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    remember_through_a_pass(home.home(), 3650);

    let path = crate::retention::record_path().expect("record path");
    assert_eq!(
        path,
        home.home().join(".clauth").join("settings-retention.json")
    );
    let mode = fs::metadata(&path)
        .expect("the record exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

// ── the put-back's record status and rank ───────────────────────────────────

/// A put-back into a live copy the record never named leaves it unnamed, so
/// the copy still cannot win a pass.
#[test]
fn a_put_back_into_an_unrecorded_copy_leaves_it_unrecorded() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    fs::remove_file(&base).expect("remove the operator file");
    let _marker = arm_runtime_marker(copy.parent().expect("runtime dir"));
    write_json(&copy, &json!({"theme": "dark", "env": {}}), t(10));

    sync();

    assert_eq!(
        read_json(&copy),
        json!({"theme": "dark", "env": {}, "cleanupPeriodDays": 3650})
    );
    let record = home
        .home()
        .join(".clauth")
        .join(SETTINGS_SYNC_WRITTEN_FILENAME);
    assert!(
        !crate::jsonsync::is_recorded(&record, &copy),
        "the copy stays out of the record"
    );
}

/// A put-back into a recorded copy Claude Code rewrote since keeps that copy's
/// rank, so the operator file's newer edit still wins the pass.
#[test]
fn a_put_back_into_a_rewritten_recorded_copy_keeps_its_rank() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "light", "env": {}}),
        t(5),
    );
    write_json(&copy, &json!({"theme": "dark", "env": {}}), t(20));
    write_json(&base, &json!({"theme": "solarized", "env": {}}), t(30));

    sync();

    let expected = json!({"cleanupPeriodDays": 3650, "theme": "solarized", "env": {}});
    assert_eq!(read_json(&base), expected, "the operator file's edit wins");
    assert_eq!(read_json(&copy), expected);
}

/// A torn retention record remembers nothing and is reported once; it is
/// rewritten only once the operator file holds a number again.
#[test]
fn a_torn_retention_record_puts_nothing_back_until_a_number_appears() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let record = crate::retention::record_path().expect("record path");
    fs::create_dir_all(record.parent().expect("record dir")).expect("mkdir ~/.clauth");
    let torn = b"{\"days\": ";
    fs::write(&record, torn).expect("tear the record");
    let keyless = json!({"theme": "light", "env": {}});
    write_json(&base, &keyless, t(1));

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
        write_json(&base, &json!({"theme": "dark", "env": {}}), t(2));
        sync();
    }

    assert_eq!(read_json(&base), json!({"theme": "dark", "env": {}}));
    assert_eq!(
        fs::read(&record).expect("read record"),
        torn,
        "kept until a number appears"
    );
    let logged = lines.snapshot();
    assert_eq!(
        logged.len(),
        1,
        "one line for a record that stays torn: {logged:?}"
    );

    write_json(
        &base,
        &json!({"cleanupPeriodDays": 90, "theme": "dark", "env": {}}),
        t(10),
    );
    sync();
    assert_eq!(retention(), remembered(90, false));
}

// ── the healed winner and the fault-tolerant pass ────────────────────────────

/// The put-back heals the winner before the delete-bound refusal, so a winner
/// lacking exactly `cleanupPeriodDays` plus three more shared keys merges
/// (deleting the three) where a keyless winner used to refuse: the restored
/// key is no deletion.
#[test]
fn a_healed_winner_lacking_three_shared_keys_still_merges() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "outputStyle": "x",
            "hooks": {}, "attribution": {}, "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &winner,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );

    sync();

    assert_eq!(
        retention(),
        remembered(3650, false),
        "the pass remembers the number"
    );
    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        "the restored key is no deletion, so three dropped keys stay under the bound"
    );
    assert_eq!(
        read_json(&winner)["cleanupPeriodDays"],
        json!(3650),
        "the winner carries the restored key"
    );
    assert!(
        backups_named(
            &home.home().join(".clauth").join("backups"),
            "settings.json"
        )
        .is_empty(),
        "a pass under the bound takes no backup"
    );
}

/// A retention-record fault (the record path is a directory) never stops the
/// merge: the pass still merges a shared-key change and still puts the
/// remembered number back into a keyless copy, and one latched line covers the
/// standing fault. The build's half of this fault lives in
/// `a_retention_record_fault_never_refuses_a_build` (runtime tests).
#[test]
fn a_retention_record_fault_never_stops_the_merge_or_a_build() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
    let record = crate::retention::record_path().expect("record path");
    fs::create_dir_all(record.parent().expect("record dir")).expect("mkdir ~/.clauth");
    fs::create_dir_all(&record).expect("make the record path a directory");

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
        sync();
    }

    assert_eq!(
        read_json(&base)["theme"],
        json!("dark"),
        "the merge still runs over the copy's newer edit"
    );
    assert_eq!(
        read_json(&copy)["cleanupPeriodDays"],
        json!(3650),
        "the copy still gets the remembered number back"
    );
    assert_eq!(
        lines.snapshot().len(),
        1,
        "one latched line for the standing record fault"
    );
}

/// The operator answering with a different valid number closes the open
/// question silently.
#[test]
fn a_new_number_while_the_question_is_open_closes_it_silently() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    remember_through_a_pass(home.home(), 3650);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    sync();
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the loss opened the question"
    );
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 90, "theme": "light", "env": {}}),
        t(30),
    );

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync();
    }

    assert_eq!(
        retention(),
        remembered(90, false),
        "a new valid number closes the question"
    );
    assert!(
        lines.snapshot().is_empty(),
        "answering by hand logs nothing"
    );
}

/// The operator file's own value, valid or not, is what a keyless copy carries:
/// `0` is never remembered, but a copy that could win the merge must still get
/// the operator file's `0` rather than go keyless and refuse the pass.
#[test]
fn a_keyless_copy_carries_the_operator_files_own_value_even_when_it_is_invalid() {
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "dark", "env": {}}),
        t(30),
    );
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 0, "theme": "light", "env": {}}),
        t(20),
    );

    sync();

    assert_eq!(
        retention(),
        remembered(3650, false),
        "0 is never remembered"
    );
    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(0),
        "a key that is present is never overwritten"
    );
    assert_eq!(
        read_json(&copy)["cleanupPeriodDays"],
        json!(0),
        "the keyless copy carries the operator file's own value"
    );
}

/// A failed put-back into the operator file skips that pass's merge only: the
/// copies still get the number, the operator file keeps its keyless bytes, the
/// open question closes again, and the failure line is latched.
#[cfg(unix)]
#[test]
fn a_failed_operator_put_back_skips_the_merge_and_closes_the_question() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    let _read_only = crate::testutil::UnwritableDir::new(&home.home().join(".claude"));

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        assert!(
            !sync_members().expect("the check runs").merged,
            "a failed operator put-back skips the merge"
        );
        assert!(
            !sync_members().expect("the check runs").merged,
            "the merge stays skipped while the put-back keeps failing"
        );
    }

    assert_eq!(
        read_json(&base),
        json!({"theme": "light", "env": {}}),
        "the operator file keeps its keyless bytes"
    );
    assert_eq!(
        read_json(&copy),
        json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        "the copy still gets the number back"
    );
    assert_eq!(
        retention(),
        remembered(3650, false),
        "no put-back landed, so the question closes again"
    );
    assert_eq!(
        lines.snapshot().len(),
        1,
        "one latched line for the failed put-back"
    );
}

/// A Claude Code rewrite of the operator file between the read and the
/// put-back's re-stat — the same race the `Err` arm guards — skips the merge:
/// the newer keyless file would win it and strip the key from the copies this
/// pass just healed, so the copies keep the key and nothing is logged.
#[test]
fn a_claude_code_rewrite_between_read_and_put_back_skips_the_merge() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "light", "env": {}}),
        t(5),
    );
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    let base_for_hook = base.clone();
    let once = std::sync::Once::new();
    crate::retention::set_rewrite_hook(Some(Box::new(move |path| {
        if path == base_for_hook.as_path() {
            once.call_once(|| {
                write_json(
                    &base_for_hook,
                    &json!({"theme": "solarized", "env": {}}),
                    t(30),
                );
            });
        }
    })));

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        assert!(
            !sync_members().expect("the check runs").merged,
            "the merge must skip while the newer keyless file stands"
        );
    }
    crate::retention::set_rewrite_hook(None);

    assert_eq!(
        read_json(&base),
        json!({"theme": "solarized", "env": {}}),
        "the newer keyless file stands"
    );
    assert_eq!(
        read_json(&copy)["cleanupPeriodDays"],
        json!(3650),
        "the copy still holds the key"
    );
    assert_eq!(
        retention(),
        remembered(3650, false),
        "no put-back landed, so the question stays closed"
    );
    assert!(
        lines.snapshot().is_empty(),
        "a refused put-back logs nothing"
    );
}

/// A hand edit landing on the operator file mid-pass, after this pass read it,
/// must still get the number back on the next check. The unparseable
/// `config.toml` pauses the merge, so nothing else rewrites the copy between
/// the passes and the mid-pass edit is the only change the next pass sees.
#[test]
fn a_hand_edit_landing_mid_pass_still_gets_the_number_back() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "dark", "env": {}}),
        t(5),
    );
    write_config(home.home(), "p1", "not = [toml");

    let base_for_hook = base.clone();
    let copy_for_hook = copy.clone();
    let once = std::sync::Once::new();
    crate::retention::set_rewrite_hook(Some(Box::new(move |path| {
        if path == copy_for_hook.as_path() {
            once.call_once(|| {
                write_json(&base_for_hook, &json!({"theme": "light", "env": {}}), t(30));
            });
        }
    })));

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync_members().expect("the first pass runs");
    }
    crate::retention::set_rewrite_hook(None);
    {
        let _capture = lines.capture_here();
        sync_members().expect("the second pass runs");
    }

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        "the operator file holds the number again"
    );
    assert_eq!(
        read_json(&copy)["cleanupPeriodDays"],
        json!(3650),
        "the live copy holds the number too"
    );
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the loss opened the question"
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| **line == put_back_line(3650))
            .count(),
        1,
        "exactly one put-back line across both passes"
    );
}

/// A keyless write landing right after the operator put-back's rename, before
/// the pass stores its fast-path key, is not the pass's own write: the next
/// check re-reads the file and puts the number back. Daemon path, no session
/// live, so nothing else would ever move the key again.
#[test]
fn a_keyless_write_right_after_the_put_back_still_gets_the_number_back() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(1),
    );
    crate::retention::reconcile().expect("the remembering pass");
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    let base_for_hook = base.clone();
    let once = std::sync::Once::new();
    crate::jsonsync::set_post_write_hook(Some(Box::new(move || {
        once.call_once(|| {
            write_json(&base_for_hook, &json!({"theme": "dark", "env": {}}), t(30));
        });
    })));
    crate::retention::reconcile().expect("the put-back pass");
    crate::jsonsync::set_post_write_hook(None);
    crate::retention::reconcile().expect("the next pass");

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "theme": "dark", "env": {}}),
        "the keyless write keeps its theme and gets the number back"
    );
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the loss opened the question"
    );
}

/// A failed operator put-back is retried by the very next check once the
/// fault clears, with nothing else moving: the failed pass cached nothing.
#[cfg(unix)]
#[test]
fn a_failed_operator_put_back_is_retried_by_the_next_check_once_the_fault_clears() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    remember_through_a_pass(home.home(), 3650);
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    {
        let _read_only = crate::testutil::UnwritableDir::new(&home.home().join(".claude"));
        crate::settings_sync::sync_once().expect("the session sync runs");
    }
    assert_eq!(
        read_json(&base),
        json!({"theme": "light", "env": {}}),
        "the failed put-back left the file as it was"
    );

    crate::settings_sync::sync_once().expect("the session sync runs");

    assert_eq!(
        read_json(&base),
        json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        "the next check retries the put-back and it lands"
    );
}

/// A live copy that stays unparseable fails `remove` with a reason that says
/// so — the key may still be in it, and nothing says it is moving — and the
/// record keeps the number.
#[test]
fn remove_errors_on_a_live_copy_that_does_not_parse_and_says_so() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(5),
    );
    std::fs::write(&copy, br#"{"cleanupPeriodDays": 36"#).expect("tear the copy");

    let started = std::time::Instant::now();
    let err = crate::retention::remove().expect_err("an unparseable copy fails the remove");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(200),
        "each re-read of the torn copy waits for a write in flight to land"
    );

    assert_eq!(
        err.to_string(),
        format!(
            "{} cannot be read or does not parse, so its cleanupPeriodDays was not removed; the key may still be there",
            copy.display()
        )
    );
    assert_eq!(
        retention(),
        remembered(3650, false),
        "the record keeps the number"
    );
}

/// Restores a directory's mode on drop, so a failing assertion never leaves a
/// sandbox directory the temp-dir cleanup cannot enter.
#[cfg(unix)]
struct ModeGuard(PathBuf, u32);

#[cfg(unix)]
impl Drop for ModeGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        drop(fs::set_permissions(
            &self.0,
            fs::Permissions::from_mode(self.1),
        ));
    }
}

/// A live copy whose stat fails for any reason but absence (its directory
/// unreadable) is not "missing": `remove` fails naming it and keeps the
/// record, or a later merge would flow the key it may still hold back.
#[cfg(unix)]
#[test]
fn remove_errors_on_a_live_copy_it_cannot_stat_and_keeps_the_record() {
    use std::os::unix::fs::PermissionsExt;
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(5),
    );
    let runtime_dir = copy.parent().expect("runtime dir").to_path_buf();
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    let restore = ModeGuard(runtime_dir, 0o700);

    let err = crate::retention::remove().expect_err("an unstattable copy fails the remove");
    drop(restore);

    assert!(
        err.to_string().contains(&copy.display().to_string()),
        "the error names the copy: {err:#}"
    );
    assert_eq!(
        retention(),
        remembered(3650, false),
        "the record keeps the number"
    );
    assert_eq!(
        read_json(&copy)["cleanupPeriodDays"],
        json!(3650),
        "the copy was never touched"
    );
}

/// Run `check` over an operator file that loses the key, gets it back, and is
/// then restored to the exact keyless bytes at the exact mtime it had before
/// the put-back (a cron `rsync -a` or `cp -p` from a source lacking the key):
/// the restored file matches every stat the put-back pass read, so only a
/// check that cached nothing after its own write sees it. `recheck` runs one
/// more check between the put-back and the restore, as the put-back's own
/// rename wakes one; without it the restore lands before any later check
/// (inside the watcher's cooldown, or on the poll fallback).
fn a_keyless_restore_at_the_old_stat_gets_the_number_back_through(check: fn(), recheck: bool) {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let keyless = json!({"theme": "light", "env": {}});
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(1),
    );
    check();
    write_json(&base, &keyless, t(20));
    check();
    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the first loss is put back"
    );
    if recheck {
        check();
    }

    write_json(&base, &keyless, t(20));
    check();

    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the restored keyless file is put back too"
    );
}

fn daemon_check() {
    crate::retention::reconcile().expect("the daemon check runs");
}

fn session_check() {
    crate::settings_sync::sync_once().expect("the session sync runs");
}

#[test]
fn a_keyless_restore_at_the_old_stat_gets_the_number_back_on_the_daemon_path() {
    a_keyless_restore_at_the_old_stat_gets_the_number_back_through(daemon_check, false);
}

#[test]
fn a_keyless_restore_at_the_old_stat_gets_the_number_back_on_the_session_path() {
    a_keyless_restore_at_the_old_stat_gets_the_number_back_through(session_check, false);
}

#[test]
fn a_keyless_restore_after_a_recheck_gets_the_number_back_on_both_paths() {
    a_keyless_restore_at_the_old_stat_gets_the_number_back_through(daemon_check, true);
    a_keyless_restore_at_the_old_stat_gets_the_number_back_through(session_check, true);
}

/// A put-back into a live copy that fails is retried once the fault clears,
/// with nothing else moving: a pass whose write failed caches nothing.
#[cfg(unix)]
#[test]
fn a_failed_copy_put_back_is_retried_once_the_fault_clears() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"theme": "dark", "env": {}}),
        t(10),
    );
    {
        let _read_only = crate::testutil::UnwritableDir::new(copy.parent().expect("runtime dir"));
        crate::settings_sync::sync_once().expect("the session sync runs");
    }
    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "a failed copy put-back never costs the operator file its number"
    );

    crate::settings_sync::sync_once().expect("the session sync runs");

    assert_eq!(
        read_json(&copy)["cleanupPeriodDays"],
        json!(3650),
        "the copy gets the number once its write can land"
    );
}

/// A keyless file that settled, gained the key by hand and lost it again to
/// its own old bytes at its old stat (a dotfile restore from a source
/// predating the hand edit) is put back: no snapshot of the settled keyless
/// pass may skip the check once the number is remembered.
#[test]
fn a_restore_to_a_settled_keyless_stat_gets_the_number_back_on_the_session_path() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let keyless = json!({"theme": "light", "env": {}});
    write_json(&base, &keyless, t(1));
    session_check();
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(10),
    );
    session_check();
    assert_eq!(
        retention(),
        remembered(3650, false),
        "the hand-added number is remembered"
    );

    write_json(&base, &keyless, t(1));
    session_check();

    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the keyless bytes back at their settled stat are put back"
    );
}

/// A number learned outside this session's own pass (a build's settle) while
/// the session had settled a keyless operator file, then the keyless bytes
/// back at that settled stat: no settings file differs from the session's
/// cached snapshot, so only the retention record in its key shows the change.
#[test]
fn a_number_learned_by_a_build_reaches_a_session_settled_on_the_keyless_file() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let keyless = json!({"theme": "light", "env": {}});
    write_json(&base, &keyless, t(1));
    session_check();
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(10),
    );
    let sync_record = crate::settings_sync::written_record_path().expect("record path");
    crate::lock::with_state_lock(|held| crate::retention::settle(held, &base, &[], &sync_record))
        .expect("the build's settle runs");
    assert_eq!(
        retention(),
        remembered(3650, false),
        "the build learned the number"
    );

    write_json(&base, &keyless, t(1));
    session_check();

    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the session puts the number back"
    );
}

/// Clears the settings sync's pre-lock hook on drop, so a failing assertion
/// never leaves it armed for a later test.
struct PreLockHookGuard;

impl Drop for PreLockHookGuard {
    fn drop(&mut self) {
        set_pre_lock_hook(None);
    }
}

/// Another process puts the number back while this session's check waits on
/// the state flock, with the question already open so the record holds still:
/// the check reads the keyed file, but the keyless stat it took before the
/// wait must not key the fast path, or the same keyless bytes restored at that
/// stat read as settled.
#[test]
fn a_put_back_landing_during_the_flock_wait_leaves_a_later_restore_to_be_put_back() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let keyless = json!({"theme": "light", "env": {}});
    remember_through_a_pass(home.home(), 3650);
    write_json(&base, &keyless, t(1));
    session_check();
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the first loss opens the question"
    );
    session_check();

    write_json(&base, &keyless, t(1));
    let sync_record = crate::settings_sync::written_record_path().expect("record path");
    set_pre_lock_hook(Some(Box::new({
        let base = base.clone();
        move || {
            crate::lock::with_state_lock(|held| {
                crate::retention::settle(held, &base, &[], &sync_record)
            })
            .expect("the other process's settle runs");
        }
    })));
    let hook = PreLockHookGuard;
    session_check();
    drop(hook);
    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the other process put the number back"
    );

    write_json(&base, &keyless, t(1));
    session_check();

    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the session puts the number back"
    );
}

/// Clears the merge engine's interleave hook on drop.
#[cfg(unix)]
struct InterleaveHookGuard;

#[cfg(unix)]
impl Drop for InterleaveHookGuard {
    fn drop(&mut self) {
        crate::jsonsync::set_interleave_hook(None);
    }
}

/// A refusal whose backup re-read fails (the operator file unreadable for that
/// read, its stat unchanged) still owes the backup: the next session check
/// reruns and takes it, though no stat moved. Unix-only: mode 000 yields the
/// read error portably there.
#[cfg(unix)]
#[test]
fn an_unreadable_home_at_the_backup_reread_is_retried_with_its_stat_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let home = HomeSandbox::new();
    let base = base_path(home.home());
    let winner = session_runtime_path(home.home(), "p1", "4242-0");
    write_json(
        &base,
        &json!({"outputStyle": "x", "hooks": {}, "attribution": {}, "theme": "dark", "env": {}}),
        t(1),
    );
    let _live = live_recorded_json(home.home(), &winner, &json!({"env": {}}), t(10));
    let backups_dir = home.home().join(".clauth").join("backups");
    crate::jsonsync::set_interleave_hook(Some(Box::new({
        let base = base.clone();
        move || {
            fs::set_permissions(&base, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        }
    })));
    let hook = InterleaveHookGuard;
    session_check();
    drop(hook);
    assert!(
        backups_named(&backups_dir, "settings.json").is_empty(),
        "no backup while the operator file is unreadable"
    );
    fs::set_permissions(&base, fs::Permissions::from_mode(0o600)).expect("chmod 600");

    session_check();

    assert_eq!(
        backups_named(&backups_dir, "settings.json").len(),
        1,
        "the owed backup lands with no stat moved"
    );
}

/// A member that keeps changing while `remove` works on it (a Claude Code
/// write landing in every read-to-re-stat window) fails the whole remove
/// naming that file, and the record keeps the number and the open question.
#[test]
fn remove_errors_on_a_member_that_keeps_moving_and_keeps_the_record() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(5),
    );
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    sync();
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the loss opened the question"
    );

    let copy_for_hook = copy.clone();
    let move_n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let move_n_for_hook = std::sync::Arc::clone(&move_n);
    crate::retention::set_rewrite_hook(Some(Box::new(move |path| {
        if path == copy_for_hook.as_path() {
            let n = move_n_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            write_json(
                &copy_for_hook,
                &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
                t(100 + n),
            );
        }
    })));

    let err = crate::retention::remove().expect_err("a member that keeps moving fails the remove");
    crate::retention::set_rewrite_hook(None);

    assert!(
        err.to_string().contains(&copy.display().to_string()),
        "the error names the moving file: {err:#}"
    );
    assert_eq!(
        move_n.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the moving copy is retried exactly the cap of three times"
    );
    assert_eq!(
        retention(),
        remembered(3650, true),
        "the record keeps the number and the open question"
    );
}

/// A member that moves once inside the read-to-re-stat window is re-read and
/// removed on the retry, so `remove` completes and the record is forgotten.
#[test]
fn remove_succeeds_on_a_member_that_moves_once() {
    let home = HomeSandbox::new();
    crate::retention::reset_latches();
    let base = base_path(home.home());
    let copy = session_runtime_path(home.home(), "p1", "4242-0");
    remember_through_a_pass(home.home(), 3650);
    let _live = live_recorded_json(
        home.home(),
        &copy,
        &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
        t(5),
    );
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    sync();

    let copy_for_hook = copy.clone();
    let once = std::sync::Once::new();
    crate::retention::set_rewrite_hook(Some(Box::new(move |path| {
        if path == copy_for_hook.as_path() {
            once.call_once(|| {
                write_json(
                    &copy_for_hook,
                    &json!({"cleanupPeriodDays": 3650, "theme": "light", "env": {}}),
                    t(30),
                );
            });
        }
    })));

    crate::retention::remove().expect("a member that moves once is removed on the retry");
    crate::retention::set_rewrite_hook(None);

    assert_eq!(
        read_json(&base),
        json!({"theme": "light", "env": {}}),
        "the operator file lost the key"
    );
    assert_eq!(
        read_json(&copy),
        json!({"theme": "light", "env": {}}),
        "the copy lost the key"
    );
    assert_eq!(
        retention(),
        crate::retention::Retention::default(),
        "the record is forgotten"
    );
}

/// `JSON.parse` yields the integer 3650 for `3650.0` and `3.65e3`, and Claude
/// Code's schema takes it, so any integral positive number is remembered — and
/// the put-back writes the integer spelling.
#[test]
fn an_integral_float_is_remembered_as_the_integer() {
    for spelled in [json!(3650.0), json!(3.65e3)] {
        let home = HomeSandbox::new();
        let base = base_path(home.home());
        write_json(
            &base,
            &json!({"cleanupPeriodDays": spelled, "env": {}}),
            t(1),
        );
        sync();
        assert_eq!(
            retention(),
            remembered(3650, false),
            "{spelled} is remembered as 3650"
        );
    }

    let home = HomeSandbox::new();
    let base = base_path(home.home());
    write_json(
        &base,
        &json!({"cleanupPeriodDays": 3650.0, "theme": "light", "env": {}}),
        t(1),
    );
    sync();
    write_json(&base, &json!({"theme": "light", "env": {}}), t(20));
    sync();
    assert_eq!(
        read_json(&base)["cleanupPeriodDays"],
        json!(3650),
        "the put-back writes the integer spelling"
    );
}
