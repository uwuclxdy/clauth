#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The uncollected-results leg: which records a resumed conversation is told
//! about, and the exact copy. Home-sandboxed so every seeded record lands in a
//! tempdir, never the real `~/.clauth/jobs`.

use super::*;
use crate::mcp::jobs::{
    Claim, Claimant, DONE_TTL_MS, claim, hold_server_marker, jobs_dir, server_owner_pid,
    server_started_at,
};
use crate::testutil::HomeSandbox;

/// Epoch ms every fixture is dated against: a real 2026 clock, so the Done TTL
/// and the corpse verdict both read a record the way they would in production.
const NOW: u64 = 1_786_000_000_000;

const SESSION: &str = "5e55-a";
const OTHER: &str = "5e55-b";

const TWO: &str = "clauth note: background delegates this session started have results waiting uncollected: `d-b` on `glm1`, `d-a` on `DS1`. `monitor` with their job_ids collects them.";

fn fire(event: &str, source: Option<&str>, agent_id: Option<&str>) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: SESSION.to_string(),
        agent_id: agent_id.map(str::to_string),
        tool_name: None,
        source: source.map(str::to_string),
        transcript: None,
    }
}

fn resume() -> Payload {
    fire("SessionStart", Some("resume"), None)
}

fn one(id: &str, profile: &str) -> String {
    format!(
        "clauth note: a background delegate this session started has a result waiting uncollected: `{id}` on `{profile}`. `monitor` with its job_id collects it."
    )
}

fn write_record(record: &serde_json::Value) {
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let id = record["job_id"].as_str().unwrap();
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(record).unwrap(),
    )
    .unwrap();
}

/// A finished background job whose result still sits in the store, finished
/// `ago` ms before [`NOW`]. The finish is the store's sort key, so it fixes the
/// order the note names jobs in: the smaller `ago`, the earlier.
fn seed_done(job_id: &str, profile: &str, host_session: Option<&str>, ago: u64) {
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": profile,
        "state": "done",
        "started_at": NOW - ago - 500,
        "done_at": NOW - ago,
        "envelope": { "result": "ok" },
    });
    if let Some(host) = host_session {
        record["host_session"] = host.into();
    }
    write_record(&record);
}

fn seed_many(n: u64) {
    for i in 0..n {
        seed_done(&format!("d-{i:02}"), "DS1", Some(SESSION), 1_000 - i);
    }
}

#[test]
fn a_resume_names_this_sessions_uncollected_results_newest_first() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 2_000);
    seed_done("d-b", "glm1", Some(SESSION), 1_000);

    assert_eq!(note_at(&resume(), NOW).as_deref(), Some(TWO));
}

#[test]
fn one_result_reads_in_the_singular() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 1_000);

    assert_eq!(note_at(&resume(), NOW), Some(one("d-a", "DS1")));
}

#[test]
fn a_collected_result_is_never_named() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 2_000);
    seed_done("d-b", "glm1", Some(SESSION), 1_000);

    assert!(matches!(claim("d-a", Claimant::Monitor), Claim::Owned(_)));
    assert_eq!(note_at(&resume(), NOW), Some(one("d-b", "glm1")));

    assert!(matches!(claim("d-b", Claimant::Hook), Claim::Owned(_)));
    assert_eq!(note_at(&resume(), NOW), None);
}

/// Another conversation's result, one whose record names no conversation, a
/// run still going, and a crashed run's tombstone (no result to collect) are
/// all left out; the one finished result this conversation started is named.
#[test]
fn only_this_sessions_finished_results_are_named() {
    let _home = HomeSandbox::new();
    seed_done("d-mine", "DS1", Some(SESSION), 4_000);
    seed_done("d-foreign", "DS2", Some(OTHER), 3_000);
    seed_done("d-unknown", "DS3", None, 2_000);
    write_record(&serde_json::json!({
        "job_id": "d-running",
        "profile": "DS4",
        "state": "running",
        "started_at": NOW - 1_000,
        "recorded_at": NOW - 1_000,
        "host_session": SESSION,
    }));
    write_record(&serde_json::json!({
        "job_id": "d-crashed",
        "profile": "DS5",
        "state": "done",
        "started_at": NOW - 600,
        "done_at": NOW - 500,
        "crashed": true,
        "host_session": SESSION,
    }));

    assert_eq!(note_at(&resume(), NOW), Some(one("d-mine", "DS1")));
}

/// The startup sweep reaps a `done` record once it is past the Done TTL, so
/// the note never offers one: `monitor` would answer it unknown. The boundary
/// is the sweep's own: exactly at the TTL it is kept.
#[test]
fn a_result_past_the_done_ttl_is_never_named() {
    let _home = HomeSandbox::new();
    seed_done("d-kept", "DS1", Some(SESSION), DONE_TTL_MS);
    seed_done("d-reaped", "DS2", Some(SESSION), DONE_TTL_MS + 1);

    assert_eq!(note_at(&resume(), NOW), Some(one("d-kept", "DS1")));
}

/// A resume names them; a compaction names them again, since it drops what
/// the resume injected. Every other fire, and any fire inside a subagent, is
/// silent.
#[test]
fn only_a_main_scope_resume_or_compaction_names_them() {
    let _home = HomeSandbox::new();
    seed_done("d-a", "DS1", Some(SESSION), 1_000);

    for (event, source, agent, want) in [
        ("SessionStart", Some("resume"), None, true),
        ("SessionStart", Some("compact"), None, true),
        ("SessionStart", Some("startup"), None, false),
        ("SessionStart", Some("clear"), None, false),
        ("SessionStart", Some("fork"), None, false),
        ("SessionStart", None, None, false),
        ("UserPromptSubmit", Some("resume"), None, false),
        ("SessionStart", Some("resume"), Some("agent-1"), false),
    ] {
        assert_eq!(
            note_at(&fire(event, source, agent), NOW),
            want.then(|| one("d-a", "DS1")),
            "{event} / {source:?} / {agent:?}"
        );
    }
}

#[test]
fn ten_results_are_all_named() {
    let _home = HomeSandbox::new();
    seed_many(10);

    assert_eq!(
        note_at(&resume(), NOW).as_deref(),
        Some(
            "clauth note: background delegates this session started have results waiting uncollected: `d-09` on `DS1`, `d-08` on `DS1`, `d-07` on `DS1`, `d-06` on `DS1`, `d-05` on `DS1`, `d-04` on `DS1`, `d-03` on `DS1`, `d-02` on `DS1`, `d-01` on `DS1`, `d-00` on `DS1`. `monitor` with their job_ids collects them."
        )
    );
}

#[test]
fn past_ten_results_the_rest_are_counted() {
    let _home = HomeSandbox::new();
    seed_many(11);

    assert_eq!(
        note_at(&resume(), NOW).as_deref(),
        Some(
            "clauth note: background delegates this session started have results waiting uncollected: `d-10` on `DS1`, `d-09` on `DS1`, `d-08` on `DS1`, `d-07` on `DS1`, `d-06` on `DS1`, `d-05` on `DS1`, `d-04` on `DS1`, `d-03` on `DS1`, `d-02` on `DS1`, `d-01` on `DS1`, and 1 more (`clauth jobs` lists them). `monitor` with their job_ids collects them."
        )
    );
}

// ── the delivery hook's session half ────────────────────────────────────────

const MINUTE_MS: u64 = 60 * 1000;

fn seed_running(job_id: &str, host_session: Option<&str>, owner_pid: u32) {
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": "DS1",
        "state": "running",
        "started_at": NOW,
        "owner_pid": owner_pid,
    });
    if let Some(host) = host_session {
        record["host_session"] = host.into();
    }
    write_record(&record);
}

fn turn_end() -> Payload {
    fire("Stop", None, None)
}

fn lease_path() -> std::path::PathBuf {
    record_path(SESSION, None).unwrap().with_extension("watch")
}

/// The awaited set keys on the host alone, owner gone or not: a record whose
/// server died is adopted so the watcher reports its result lost.
#[test]
fn own_jobs_are_this_sessions_by_host_alone() {
    let _home = HomeSandbox::new();
    seed_running("d-own", Some(SESSION), 0);
    seed_running("d-peer", Some(OTHER), 0);
    seed_running("d-untied", None, 0);
    seed_running("d-dead", Some(SESSION), 4242);
    seed_done("d-finished", "DS1", Some(SESSION), 1_000);
    seed_done("d-peer-finished", "DS1", Some(OTHER), 1_000);

    let mut own = own_jobs(SESSION, NOW);
    own.sort();
    assert_eq!(own, ["d-dead", "d-finished", "d-own"]);
}

/// A running record silent past the store's corpse window reads dead to the
/// store's own sweep, ownerless or not — but the watcher still adopts it:
/// what the store calls dead, the wake reports as lost, so no turn end drops
/// it silently. Within the window it is awaited as before — the silence is
/// the corpse rule, not the absence of an owner.
#[test]
fn an_ownerless_record_silent_past_the_window_is_awaited_as_an_orphan() {
    let _home = HomeSandbox::new();
    seed_running("d-silent", Some(SESSION), 0);

    assert_eq!(
        own_jobs(SESSION, NOW),
        ["d-silent"],
        "within the window it is still awaited"
    );
    assert_eq!(
        own_jobs(SESSION, NOW + 2 * jobs::RUNNING_TTL_MS),
        ["d-silent"],
        "past the window it is adopted as orphaned, to report its result lost"
    );
}

/// A hung job still owned by a LIVE server reads live to the store — the
/// store never reaps a live owner's record — but the watcher's own rule is the
/// silence window: a silent record is not awaited whatever its owner, or the
/// 55 min wakes would run forever on a job that only a live-owner sweep never
/// touches.
#[test]
fn a_silent_record_of_a_live_owner_is_not_awaited() {
    let _home = HomeSandbox::new();
    let _marker = hold_server_marker().expect("hold the marker");
    let mut record = serde_json::json!({
        "job_id": "d-hung",
        "profile": "DS1",
        "state": "running",
        "started_at": 1,
        "owner_pid": server_owner_pid(),
        "owner_started_at": server_started_at(),
    });
    record["host_session"] = SESSION.into();
    write_record(&record);

    assert_eq!(own_jobs(SESSION, NOW), Vec::<String>::new());
}

/// The ids `own_jobs` hands on come from the store's own files, which the
/// user's uid can edit, so a record whose id spells a path must never reach
/// `jobs::read` or `jobs::claim`: the same `is_safe_job_id` gate tool input
/// gets applies at the boundary.
#[test]
fn a_store_record_with_an_unsafe_id_is_never_awaited() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-bad.json"),
        serde_json::json!({
            "job_id": "../evil",
            "profile": "DS1",
            "state": "running",
            "started_at": NOW,
            "host_session": SESSION,
        })
        .to_string(),
    )
    .unwrap();
    seed_running("d-safe", Some(SESSION), 0);

    assert_eq!(own_jobs(SESSION, NOW), ["d-safe"]);
}

/// A blocking run is never awaited (its caller holds the join); a crashed
/// done tombstone is — the wake reports its result lost.
#[test]
fn a_blocking_run_is_not_awaited_but_a_crashed_tombstone_is() {
    let _home = HomeSandbox::new();
    let dir = jobs_dir().unwrap();
    seed_running("d-blocking", Some(SESSION), 0);
    std::fs::rename(
        dir.join("d-blocking.json"),
        dir.join("d-blocking.live.json"),
    )
    .unwrap();
    write_record(&serde_json::json!({
        "job_id": "d-crashed",
        "profile": "DS1",
        "state": "done",
        "started_at": NOW - 2_000,
        "done_at": NOW - 1_000,
        "crashed": true,
        "host_session": SESSION,
    }));

    assert_eq!(own_jobs(SESSION, NOW), ["d-crashed"]);
}

#[test]
fn idle_wake_due_counts_from_the_turn_end_to_the_window() {
    let window = 55 * MINUTE_MS;
    let turn_end = 1_000_000_000;
    for (turn_end, now, due) in [
        (None, turn_end + 10 * window, false),
        (Some(turn_end), turn_end, false),
        (Some(turn_end), turn_end + 3_299_999, false),
        (Some(turn_end), turn_end + 3_300_000, true),
        (Some(turn_end), turn_end + 3_300_001, true),
        // A clock stepped behind the stamp.
        (Some(turn_end), turn_end - 1, false),
    ] {
        assert_eq!(
            idle_wake_due(turn_end, now, window),
            due,
            "{turn_end:?} at {now}"
        );
    }
}

/// Claude Code 2.1.319's own order for the main conversation's prompt-cache
/// TTL: `FORCE_PROMPT_CACHING_5M`, then `CLAUDE_CODE_PROMPT_CACHE_TTL`, then
/// `promptCacheTtl`, then `ENABLE_PROMPT_CACHING_1H`, then the account: 1 h on
/// a subscription within its usage limits, 5 min otherwise. A value that is no
/// TTL falls through to the next step; the two flags read `1`, `true`, `yes`
/// or `on`, case-insensitive and trimmed.
#[test]
fn the_main_cache_ttl_follows_claude_codes_order() {
    use CacheTtl::{FiveMinutes as M5, OneHour as H1};
    use TtlSource::{Automatic, Enable1hEnv, Env, Force5mEnv, Setting};
    const SUB: CacheAccount = CacheAccount::Subscription { overage: false };
    const OVER: CacheAccount = CacheAccount::Subscription { overage: true };
    const OTHER: CacheAccount = CacheAccount::Other;
    let row = |force_5m, env_ttl, setting, enable_1h, account| CacheInputs {
        force_5m,
        env_ttl,
        setting,
        enable_1h,
        account,
    };
    for (inputs, want) in [
        (
            row(Some("1"), Some("1h"), Some("1h"), Some("1"), SUB),
            (M5, Force5mEnv),
        ),
        (row(Some(" TRUE "), None, None, None, SUB), (M5, Force5mEnv)),
        (row(Some("0"), Some("1h"), None, None, OTHER), (H1, Env)),
        (row(None, Some("5m"), Some("1h"), None, SUB), (M5, Env)),
        (row(None, Some("1h"), None, None, OTHER), (H1, Env)),
        (
            row(None, Some("2h"), Some("1h"), None, OTHER),
            (H1, Setting),
        ),
        (row(None, None, Some("5m"), Some("1"), SUB), (M5, Setting)),
        (row(None, None, Some("1h"), None, OVER), (H1, Setting)),
        (
            row(None, None, Some("bogus"), Some("yes"), OTHER),
            (H1, Enable1hEnv),
        ),
        (row(None, None, None, Some("on"), OVER), (H1, Enable1hEnv)),
        (row(None, None, None, Some("off"), OTHER), (M5, Automatic)),
        (row(None, None, None, None, SUB), (H1, Automatic)),
        (row(None, None, None, None, OVER), (M5, Automatic)),
        (row(None, None, None, None, OTHER), (M5, Automatic)),
    ] {
        assert_eq!(main_cache_ttl(&inputs), want, "{inputs:?}");
    }
}

#[test]
fn the_ttl_setting_reads_the_sessions_own_config_dir() {
    let home = HomeSandbox::new();
    let _env = crate::testutil::EnvPin::new(&home, &[("CLAUDE_CODE_PROMPT_CACHE_TTL", None)]);
    let tree = home.home().join("tree");
    let global = home.home().join(".claude");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(global.join("settings.json"), r#"{"promptCacheTtl":"1h"}"#).unwrap();
    assert!(prompt_cache_is_1h(), "the global settings answer alone");

    let _session = crate::testutil::ConfigDirSandbox::new(&home, &tree);
    assert!(!prompt_cache_is_1h(), "a session tree with no file says no");
    std::fs::write(tree.join("settings.json"), r#"{"promptCacheTtl":"1h"}"#).unwrap();
    assert!(prompt_cache_is_1h());
    std::fs::write(tree.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
    assert!(!prompt_cache_is_1h());
    std::fs::write(tree.join("settings.json"), "not json").unwrap();
    assert!(!prompt_cache_is_1h(), "a torn file reads as no");
}

#[test]
fn the_env_ttl_decides_the_real_read() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(global.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();

    let _env = crate::testutil::EnvPin::new(
        &home,
        &[(
            "CLAUDE_CODE_PROMPT_CACHE_TTL",
            Some(std::ffi::OsStr::new("1h")),
        )],
    );
    assert!(prompt_cache_is_1h());
}

/// The env hygiene every real-read test needs: the TTL vars and the billing
/// vars unset, so the fixture alone decides the account leg.
fn pin_no_ttl_env<'a>(home: &'a HomeSandbox) -> crate::testutil::EnvPin<'a> {
    crate::testutil::EnvPin::new(
        home,
        &[
            ("CLAUDE_CONFIG_DIR", None),
            ("ANTHROPIC_API_KEY", None),
            ("ANTHROPIC_AUTH_TOKEN", None),
            ("ANTHROPIC_BASE_URL", None),
            ("CLAUDE_CODE_USE_BEDROCK", None),
            ("CLAUDE_CODE_USE_VERTEX", None),
            ("CLAUDE_CODE_USE_FOUNDRY", None),
            ("FORCE_PROMPT_CACHING_5M", None),
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", None),
            ("ENABLE_PROMPT_CACHING_1H", None),
        ],
    )
}

/// A clauth account the session resolves to: an OAuth pair linked as the
/// global credentials and registered in the app state. `maxed` plants a
/// cached usage reading with a live window pinned at the 100% cap.
fn seed_account(profile: crate::profile::Profile, maxed: bool) {
    crate::testutil::persist_profiles(std::slice::from_ref(&profile));
    crate::profile::save_app_state(&crate::profile::AppState {
        active_profile: Some(profile.name.clone()),
        profiles: vec![profile.name.clone()],
        ..crate::profile::AppState::default()
    })
    .expect("save state");
    crate::testutil::write_linked_credentials("rt-work");
    if maxed {
        let now = crate::usage::now_epoch_secs();
        let info = crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now + 7200)),
            }),
            ..crate::usage::UsageInfo::default()
        };
        crate::profile_cache::write_profile_cache(
            &profile.name,
            crate::profile_cache::USAGE_CACHE_FILE,
            &info,
        );
    }
}

/// The two flag env vars each decide the real read on their own: the force
/// flag outranks a 1h TTL, and the enable flag is a 1h of its own.
#[test]
fn the_ttl_flag_env_vars_decide_the_real_read() {
    let home = HomeSandbox::new();
    let force = crate::testutil::EnvPin::new(
        &home,
        &[
            ("FORCE_PROMPT_CACHING_5M", Some(std::ffi::OsStr::new("1"))),
            (
                "CLAUDE_CODE_PROMPT_CACHE_TTL",
                Some(std::ffi::OsStr::new("1h")),
            ),
        ],
    );
    assert!(
        !prompt_cache_is_1h(),
        "FORCE_PROMPT_CACHING_5M outranks the 1h TTL"
    );
    drop(force);

    let _enable = crate::testutil::EnvPin::new(
        &home,
        &[(
            "ENABLE_PROMPT_CACHING_1H",
            Some(std::ffi::OsStr::new("yes")),
        )],
    );
    assert!(prompt_cache_is_1h(), "ENABLE_PROMPT_CACHING_1H reads 1h");
}

/// An env value that is no TTL falls through to the next step, never hides
/// the setting.
#[test]
fn an_invalid_env_ttl_falls_through_to_the_setting() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("settings.json"), r#"{"promptCacheTtl":"1h"}"#).unwrap();
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[(
            "CLAUDE_CODE_PROMPT_CACHE_TTL",
            Some(std::ffi::OsStr::new("2h")),
        )],
    );

    assert!(prompt_cache_is_1h());
}

/// A clauth account the session resolves to, reading as an OAuth login with
/// no api key and no base URL and no billing env, is a subscription: the
/// automatic step reads 1h.
#[test]
fn a_subscription_account_reads_as_a_wake() {
    let home = HomeSandbox::new();
    seed_account(crate::testutil::oauth_profile("work", "rt-work"), false);
    let _env = pin_no_ttl_env(&home);

    assert!(prompt_cache_is_1h());
}

/// The same account with an api key is not a subscription: the automatic
/// step reads 5m.
#[test]
fn an_api_key_account_reads_as_no_wake() {
    let home = HomeSandbox::new();
    let mut profile = crate::testutil::oauth_profile("work", "rt-work");
    profile.api_key = Some("sk-ant-test".to_string());
    seed_account(profile, false);
    let _env = pin_no_ttl_env(&home);

    assert!(!prompt_cache_is_1h());
}

/// The automatic 1h is the account's usage: a live window pinned at the cap
/// reads the subscription as in overage, so the automatic step reads 5m.
#[test]
fn an_overage_subscription_reads_as_no_wake() {
    let home = HomeSandbox::new();
    seed_account(crate::testutil::oauth_profile("work", "rt-work"), true);
    let _env = pin_no_ttl_env(&home);

    assert!(!prompt_cache_is_1h());
}

/// The long window (the 7d) pinned at the cap reads as overage too: the wake
/// follows the same spent predicate, whatever window is maxed.
#[test]
fn a_long_window_overage_subscription_reads_as_no_wake() {
    let home = HomeSandbox::new();
    let profile = crate::testutil::oauth_profile("work", "rt-work");
    let name = profile.name.clone();
    seed_account(profile, false);
    let now = crate::usage::now_epoch_secs();
    let info = crate::usage::UsageInfo {
        seven_day: Some(crate::usage::UsageWindow {
            utilization: 100.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 7200)),
        }),
        ..crate::usage::UsageInfo::default()
    };
    crate::profile_cache::write_profile_cache(&name, crate::profile_cache::USAGE_CACHE_FILE, &info);
    let _env = pin_no_ttl_env(&home);

    assert!(!prompt_cache_is_1h());
}

/// Overage changes only the automatic step: an explicit 1h setting stays a
/// wake in overage.
#[test]
fn an_explicit_1h_setting_stays_a_wake_in_overage() {
    let home = HomeSandbox::new();
    seed_account(crate::testutil::oauth_profile("work", "rt-work"), true);
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("settings.json"), r#"{"promptCacheTtl":"1h"}"#).unwrap();
    let _env = pin_no_ttl_env(&home);

    assert!(prompt_cache_is_1h());
}

/// A wrong-typed subagent key drops its own value alone: the main TTL beside
/// it still reads, so the wake follows it and the agent row reads auto.
#[test]
fn a_wrong_typed_subagent_ttl_drops_only_its_own_key() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.json"),
        r#"{"promptCacheTtl":"1h","subagentPromptCacheTtl":3600}"#,
    )
    .unwrap();
    let _env = pin_no_ttl_env(&home);

    assert!(
        prompt_cache_is_1h(),
        "the main TTL beside the bad key still reads"
    );
    let view = CacheView::read();
    assert_eq!(view.main_setting.as_deref(), Some("1h"));
    assert_eq!(view.subagent_setting, None, "the bad key alone reads unset");
    assert_eq!(
        view.subagent_ttl(),
        (CacheTtl::FiveMinutes, TtlSource::Automatic),
        "the agent TTL reads auto"
    );
}

/// An OAuth pair behind an operator-authored `[env] ANTHROPIC_BASE_URL` routes
/// requests elsewhere: the automatic step reads no subscription, so no wake.
#[test]
fn an_env_base_url_profile_reads_as_no_wake() {
    let home = HomeSandbox::new();
    let mut profile = crate::testutil::oauth_profile("work", "rt-work");
    profile.env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://proxy.example".to_string(),
    );
    seed_account(profile, false);
    let _env = pin_no_ttl_env(&home);

    assert!(!prompt_cache_is_1h());
}

/// A profile the session resolves to with no login holds nothing that reads
/// subscription usage: the automatic step reads no wake.
#[test]
fn a_login_less_profile_reads_as_no_wake() {
    let home = HomeSandbox::new();
    seed_account(
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("work")),
        false,
    );
    let _env = pin_no_ttl_env(&home);

    assert!(!prompt_cache_is_1h());
}

/// The session env's own ANTHROPIC_BASE_URL routes the host elsewhere, login
/// or not: the automatic step reads no wake.
#[test]
fn a_session_env_base_url_reads_as_no_wake() {
    let home = HomeSandbox::new();
    seed_account(crate::testutil::oauth_profile("work", "rt-work"), false);
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            ("CLAUDE_CONFIG_DIR", None),
            ("ANTHROPIC_API_KEY", None),
            ("ANTHROPIC_AUTH_TOKEN", None),
            ("CLAUDE_CODE_USE_BEDROCK", None),
            ("CLAUDE_CODE_USE_VERTEX", None),
            ("CLAUDE_CODE_USE_FOUNDRY", None),
            ("FORCE_PROMPT_CACHING_5M", None),
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", None),
            ("ENABLE_PROMPT_CACHING_1H", None),
            (
                "ANTHROPIC_BASE_URL",
                Some(std::ffi::OsStr::new("https://proxy.example")),
            ),
        ],
    );

    assert!(!prompt_cache_is_1h());
}

#[test]
fn a_prompt_clears_the_count_and_only_a_main_prompt_does() {
    let _home = HomeSandbox::new();
    drop(mark_turn_end(&turn_end(), NOW));
    assert_eq!(turn_ended_at(SESSION), Some(NOW));

    note_turn_start(&fire("UserPromptSubmit", None, Some("agent-1")));
    note_turn_start(&fire("SessionStart", Some("resume"), None));
    assert_eq!(turn_ended_at(SESSION), Some(NOW));

    note_turn_start(&fire("UserPromptSubmit", None, None));
    assert_eq!(turn_ended_at(SESSION), None);
}

/// The stamp rides a record four other legs own, and a wholesale write would
/// reset what each of them told this conversation.
#[test]
fn the_turn_end_stamp_keeps_the_rest_of_the_record() {
    let _home = HomeSandbox::new();
    let path = record_path(SESSION, None).unwrap();
    crate::profile::mkdir_700(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"told":"DS1","last_note":"clauth note: a note","resume_seq":7}"#,
    )
    .unwrap();

    drop(mark_turn_end(&turn_end(), NOW));
    clear_turn_end(SESSION);

    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["told"], "DS1");
    assert_eq!(stored["last_note"], "clauth note: a note");
    assert_eq!(stored["resume_seq"], 7);
    assert_eq!(stored["turn_ended_at"], serde_json::Value::Null);
}

#[test]
fn a_prompt_with_nothing_out_writes_nothing() {
    let home = HomeSandbox::new();

    note_turn_start(&fire("UserPromptSubmit", None, None));

    assert!(!home.home().join(".clauth").join("conversations").exists());
}

#[test]
fn the_lease_admits_one_watcher_until_it_goes() {
    let _home = HomeSandbox::new();

    let held = WatchLease::take(SESSION).expect("a free session");
    assert_eq!(held.session_id(), SESSION);
    assert!(WatchLease::take(SESSION).is_none(), "one watcher at a time");
    assert!(
        WatchLease::take(OTHER).is_some(),
        "another session has its own"
    );

    drop(held);
    assert!(
        WatchLease::take(SESSION).is_some(),
        "the flock left with it"
    );
}

/// A lease that cannot exist must not cost a delivery: `jobs::claim` still
/// holds a second watcher to one, and a job with no watcher has no such net.
#[test]
fn a_lease_that_cannot_open_still_watches() {
    let _home = HomeSandbox::new();
    std::fs::create_dir_all(lease_path()).unwrap();

    assert!(WatchLease::take(SESSION).is_some());
}

#[test]
#[cfg(unix)]
fn the_lease_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let _home = HomeSandbox::new();

    let _held = WatchLease::take(SESSION).expect("a free session");

    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode(&lease_path()) & 0o777, 0o600);
    assert_eq!(mode(lease_path().parent().unwrap()) & 0o777, 0o700);
}

#[test]
fn the_sweep_reaps_a_lease_whose_conversation_is_gone() {
    let _home = HomeSandbox::new();
    drop(WatchLease::take(SESSION));
    assert!(lease_path().exists());

    crate::hook_note::gc_conversation_records();

    assert!(!lease_path().exists());
}

#[test]
fn the_sweep_keeps_a_lease_beside_its_record() {
    let _home = HomeSandbox::new();
    drop(mark_turn_end(&turn_end(), NOW));

    crate::hook_note::gc_conversation_records();

    assert!(lease_path().exists());
}

#[test]
fn the_sweep_never_reaps_a_held_lease() {
    let _home = HomeSandbox::new();
    let held = WatchLease::take(SESSION).expect("a free session");

    crate::hook_note::gc_conversation_records();

    assert!(lease_path().exists());
    assert!(
        WatchLease::take(SESSION).is_none(),
        "the next taker still meets the holder's inode",
    );
    drop(held);
}

/// The reported-lost once lives in a sidecar beside the record, never the
/// lease file's content: on Windows the lease's lock is mandatory
/// (`LockFileEx`), so bytes inside it are unreadable to a second watcher. A
/// watcher holding the lease still records and reads the set, and the lease
/// file itself stays empty — asserted by length through a metadata stat,
/// never a second handle's read, which the very lock this test holds would
/// refuse on Windows.
#[test]
fn the_reported_lost_once_lives_beside_a_held_lease() {
    let _home = HomeSandbox::new();
    let held = WatchLease::take(SESSION).expect("a free session");

    record_lost(SESSION, &["d-dead".to_string()]);
    assert_eq!(
        reported_lost(SESSION),
        ["d-dead"],
        "the second watcher reads the once while the lease is held"
    );
    assert_eq!(
        std::fs::metadata(lease_path())
            .expect("stat the lease")
            .len(),
        0,
        "the lease file carries nothing"
    );

    drop(held);
}

/// The sweep reaps the sidecar exactly when it reaps the lease: the once dies
/// with the conversation it names.
#[test]
fn the_sweep_reaps_the_reported_lost_sidecar_with_the_lease() {
    let _home = HomeSandbox::new();
    drop(WatchLease::take(SESSION));
    record_lost(SESSION, &["d-dead".to_string()]);
    let sidecar = lease_path().with_extension("lost");
    assert!(sidecar.exists());

    crate::hook_note::gc_conversation_records();

    assert!(!lease_path().exists());
    assert!(!sidecar.exists(), "the sidecar is reaped with the lease");
}

// ── the moved-session judge ────────────────────────────────────────────────

/// The record [`crate::session_record::session_record`] reads: the host's
/// CURRENT session id, keyed by the pid `CLAUDE_PID` names.
fn write_host_record(home: &HomeSandbox, pid: &str, session_id: &str) {
    let dir = home.home().join(".claude").join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{pid}.json")),
        serde_json::json!({"sessionId": session_id, "pid": pid.parse::<u32>().unwrap()})
            .to_string(),
    )
    .unwrap();
}

#[test]
fn the_host_session_id_off_claude_pid_tells_whether_the_id_moved() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            ("CLAUDE_PID", Some(std::ffi::OsStr::new("1234"))),
            ("CLAUDE_CONFIG_DIR", Some(claude.as_os_str())),
        ],
    );

    assert!(!host_session_moved(SESSION), "no record reads as not moved");

    write_host_record(&home, "1234", SESSION);
    assert!(!host_session_moved(SESSION), "the record names the same id");

    write_host_record(&home, "1234", OTHER);
    assert!(host_session_moved(SESSION), "the record names a moved id");
    assert!(!host_session_moved(OTHER), "the moved id is the host's own");

    std::fs::write(claude.join("sessions").join("1234.json"), "not json").unwrap();
    assert!(
        !host_session_moved(SESSION),
        "an unreadable record reads as not moved",
    );

    write_host_record(&home, "999", SESSION);
    assert!(
        !host_session_moved(SESSION),
        "a record under another pid reads as not moved",
    );
}

#[test]
fn no_claude_pid_reads_as_not_moved() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    write_host_record(&home, "1234", OTHER);
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            ("CLAUDE_PID", None),
            ("CLAUDE_CONFIG_DIR", Some(claude.as_os_str())),
        ],
    );

    assert!(!host_session_moved(SESSION));
}
