use super::*;

use crate::claude::{
    FailLiveSettingsWrite, apply_profile_to_claude_settings, build_claude_settings_json,
    claude_settings_path, effective_effort, live_effort, release_saved_base_env,
};
use crate::lock::with_state_lock;
use crate::profile::{atomic_write, atomic_write_600, read_json_file};
use crate::testutil::HomeSandbox;

fn profile(
    name: &str,
    model: Option<&str>,
    effort: Option<&str>,
    table: &[(&str, &str)],
) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.models.default = model.map(str::to_string);
    p.effort = effort.map(str::to_string);
    p.model_effort = table
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    p
}

/// [`selected_effort`]'s decided level, the value the carrier hands over.
fn selected_level(profile: &Profile) -> Option<String> {
    let selected = selected_effort(profile)?;
    selected.level
}

fn seed_live(json: serde_json::Value) {
    atomic_write(
        &claude_settings_path().expect("settings path"),
        serde_json::to_string(&json).expect("serialize"),
    )
    .expect("seed live settings");
}

fn live() -> serde_json::Value {
    read_json_file(&claude_settings_path().expect("settings path")).expect("live settings")
}

fn level(settings: &serde_json::Value, id: &str) -> Option<String> {
    settings["modelSettings"][id]["effortLevel"]
        .as_str()
        .map(str::to_string)
}

fn switch_off() {
    with_state_lock(|held| release_saved_base_env(&[], held)).expect("switch off");
}

#[test]
fn only_a_single_model_row_selects_a_model() {
    let mut models = ModelSettings::default();
    assert_eq!(selected_model(&models), None);
    for none in ["default", "opusplan", "OpusPlan", "  "] {
        models.default = Some(none.to_string());
        assert_eq!(selected_model(&models), None, "{none:?}");
    }
    models.default = Some("sonnet".to_string());
    assert_eq!(selected_model(&models), Some("sonnet"));
    models.default = Some("claude-opus-4-8[1m]".to_string());
    assert_eq!(selected_model(&models), Some("claude-opus-4-8[1m]"));
}

#[test]
fn the_scalar_is_the_selected_models_level_until_the_table_names_it() {
    let p = profile("p", Some("opus"), Some("medium"), &[("sonnet", "xhigh")]);
    assert_eq!(
        resolved(&p),
        BTreeMap::from([
            ("opus".to_string(), "medium".to_string()),
            ("sonnet".to_string(), "xhigh".to_string()),
        ])
    );
    assert_eq!(effective_effort(&p), "", "the carrier would pin sonnet too");

    let p = profile("p", Some("opus"), Some("medium"), &[("opus", "low")]);
    assert_eq!(
        resolved(&p),
        BTreeMap::from([("opus".to_string(), "low".to_string())]),
        "the table's own entry wins over the scalar"
    );
}

#[test]
fn an_account_wide_model_row_keeps_the_scalar_on_the_carrier() {
    for model in [None, Some("opusplan")] {
        let p = profile("p", model, Some("high"), &[("sonnet", "xhigh")]);
        assert_eq!(effective_effort(&p), "high", "{model:?}");
        assert_eq!(
            resolved(&p),
            BTreeMap::from([("sonnet".to_string(), "xhigh".to_string())]),
            "{model:?}: only the table is per-model"
        );
    }
}

#[test]
fn max_or_a_custom_scalar_stays_account_wide_under_a_selected_model() {
    for scalar in ["max", "turbo"] {
        let p = profile("p", Some("opus"), Some(scalar), &[]);
        assert_eq!(effective_effort(&p), scalar);
        assert!(resolved(&p).is_empty(), "{scalar} has no per-model form");
    }
}

#[test]
fn an_env_override_still_wins_and_a_table_value_claude_code_drops_is_skipped() {
    let mut p = profile(
        "p",
        Some("opus"),
        Some("high"),
        &[("sonnet", "max"), ("haiku", "low")],
    );
    p.env
        .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "low".to_string());
    assert_eq!(effective_effort(&p), "low");
    assert_eq!(invalid(&p), vec!["sonnet"]);
    assert_eq!(
        resolved(&p),
        BTreeMap::from([
            ("haiku".to_string(), "low".to_string()),
            ("opus".to_string(), "high".to_string()),
        ])
    );
}

#[test]
fn a_tier_with_a_models_row_preserves_its_configured_id() {
    let mut p = profile("p", Some("opus"), Some("high"), &[("sonnet", "low")]);
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    assert_eq!(
        resolved(&p),
        BTreeMap::from([
            (
                "claude-opus-4-5-20251101[1m]".to_string(),
                "high".to_string()
            ),
            ("sonnet".to_string(), "low".to_string()),
        ]),
        "a tier with no row stays its alias"
    );
}

#[test]
fn a_runtime_build_writes_each_level_and_keeps_the_models_other_keys() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({
        "modelSettings": {"opus": {"autoCompactWindow": 200000}, "haiku": {"effortLevel": "low"}}
    }));
    let p = profile("p", Some("opus"), Some("high"), &[("sonnet", "xhigh")]);
    let built: serde_json::Value = serde_json::from_str(
        &build_claude_settings_json(Some(&claude_settings_path().unwrap()), &p, &[]).unwrap(),
    )
    .unwrap();
    assert_eq!(level(&built, "opus").as_deref(), Some("high"));
    assert_eq!(level(&built, "sonnet").as_deref(), Some("xhigh"));
    assert_eq!(
        level(&built, "haiku").as_deref(),
        Some("low"),
        "an unpinned level stays"
    );
    assert_eq!(built["modelSettings"]["opus"]["autoCompactWindow"], 200000);
    assert_eq!(built["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "");
}

#[test]
fn a_switch_saves_the_users_level_and_a_switch_away_puts_it_back() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    let a = profile("a", Some("opus"), Some("high"), &[]);
    apply_profile_to_claude_settings(&a, &[]).unwrap();
    assert_eq!(level(&live(), "opus").as_deref(), Some("high"));

    let b = profile("b", None, None, &[]);
    apply_profile_to_claude_settings(&b, &[]).unwrap();
    assert_eq!(level(&live(), "opus").as_deref(), Some("low"));
    let record = crate::claude::saved_model_effort().unwrap();
    assert!(record.is_empty(), "the cover ended, so its entry retired");
}

#[test]
fn a_switch_off_puts_back_every_covered_level_and_removes_one_the_user_never_had() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    let a = profile("a", Some("opus"), Some("high"), &[("sonnet", "xhigh")]);
    apply_profile_to_claude_settings(&a, &[]).unwrap();
    switch_off();
    let after = live();
    assert_eq!(level(&after, "opus").as_deref(), Some("low"));
    assert!(
        after["modelSettings"].get("sonnet").is_none(),
        "no level of the user's own: the key leaves with clauth's: {after}"
    );
}

#[test]
fn a_level_the_user_changes_while_covered_is_kept_on_a_switch_away() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    apply_profile_to_claude_settings(&profile("a", Some("opus"), Some("high"), &[]), &[]).unwrap();
    let mut changed = live();
    changed["modelSettings"]["opus"]["effortLevel"] = "medium".into();
    seed_live(changed);
    apply_profile_to_claude_settings(&profile("b", None, None, &[]), &[]).unwrap();
    assert_eq!(level(&live(), "opus").as_deref(), Some("medium"));
}

#[test]
fn a_takeover_whose_live_write_failed_never_loses_the_users_level() {
    let home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    apply_profile_to_claude_settings(&profile("a", Some("opus"), Some("high"), &[]), &[]).unwrap();
    {
        let _fault = FailLiveSettingsWrite::new(&home);
        apply_profile_to_claude_settings(&profile("b", Some("opus"), Some("medium"), &[]), &[])
            .expect_err("the live write fails after the record's pre-write");
    }
    assert_eq!(
        level(&live(), "opus").as_deref(),
        Some("high"),
        "a's level is still live"
    );
    apply_profile_to_claude_settings(&profile("c", None, None, &[]), &[]).unwrap();
    assert_eq!(
        level(&live(), "opus").as_deref(),
        Some("low"),
        "a's live level reads as clauth's through `pending`, so the user's base survives"
    );
}

#[test]
fn a_session_started_after_a_switch_never_inherits_the_switched_accounts_level() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    apply_profile_to_claude_settings(&profile("a", Some("opus"), Some("high"), &[]), &[]).unwrap();
    let b = profile("b", None, None, &[]);
    let built: serde_json::Value = serde_json::from_str(
        &build_claude_settings_json(Some(&claude_settings_path().unwrap()), &b, &[]).unwrap(),
    )
    .unwrap();
    assert_eq!(level(&built, "opus").as_deref(), Some("low"));
}

#[test]
fn a_session_started_after_a_switch_keeps_a_level_the_user_set_since() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    apply_profile_to_claude_settings(&profile("a", Some("opus"), Some("high"), &[]), &[]).unwrap();
    let mut changed = live();
    changed["modelSettings"]["opus"]["effortLevel"] = "medium".into();
    seed_live(changed);
    let built: serde_json::Value = serde_json::from_str(
        &build_claude_settings_json(
            Some(&claude_settings_path().unwrap()),
            &profile("b", None, None, &[]),
            &[],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(level(&built, "opus").as_deref(), Some("medium"));
}

#[test]
fn a_record_that_does_not_parse_refuses_the_build_instead_of_guessing() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "high"}}}));
    atomic_write_600(&crate::claude::saved_base_env_path().unwrap(), "{not json").unwrap();
    let err = build_claude_settings_json(
        Some(&claude_settings_path().unwrap()),
        &profile("b", None, None, &[]),
        &[],
    )
    .expect_err("a torn record could hide another account's level");
    assert!(format!("{err:#}").contains("base-env.json"), "{err:#}");
    apply_profile_to_claude_settings(&profile("b", None, None, &[]), &[])
        .expect_err("a live apply refuses too");
    assert_eq!(
        level(&live(), "opus").as_deref(),
        Some("high"),
        "nothing was written"
    );
}

#[test]
fn an_account_with_only_a_table_creates_the_live_settings() {
    let _home = HomeSandbox::new();
    let path = claude_settings_path().unwrap();
    let _ = std::fs::remove_file(&path);
    apply_profile_to_claude_settings(&profile("a", None, None, &[("sonnet", "xhigh")]), &[])
        .unwrap();
    assert_eq!(level(&live(), "sonnet").as_deref(), Some("xhigh"));
}

#[test]
fn a_swap_hands_back_the_outgoing_accounts_levels_and_writes_the_incoming_ones() {
    let mut runtime = serde_json::json!({"modelSettings": {
        "opus": {"effortLevel": "high", "autoCompactWindow": 200000},
        "claude-opus-4-5": {"effortLevel": "xhigh"},
        "haiku": {"effortLevel": "medium"},
    }});
    let fresh = serde_json::json!({"modelSettings": {
        "opus": {"effortLevel": "low"},
        "haiku": {"effortLevel": "low"},
    }});
    let outgoing = BTreeMap::from([
        ("opus".to_string(), "high".to_string()),
        ("claude-opus-4-5".to_string(), "xhigh".to_string()),
    ]);
    let incoming = BTreeMap::from([("sonnet".to_string(), "xhigh".to_string())]);
    hand_over(
        runtime.as_object_mut().unwrap(),
        fresh.as_object().unwrap(),
        &incoming,
        Some(&outgoing),
    )
    .unwrap();
    assert_eq!(
        level(&runtime, "opus").as_deref(),
        Some("low"),
        "back to the user's own"
    );
    assert_eq!(
        runtime["modelSettings"]["opus"]["autoCompactWindow"],
        200000
    );
    assert!(
        runtime["modelSettings"].get("claude-opus-4-5").is_none(),
        "no level of the user's own: the outgoing key leaves: {runtime}"
    );
    assert_eq!(
        level(&runtime, "haiku").as_deref(),
        Some("medium"),
        "never the outgoing's"
    );
    assert_eq!(level(&runtime, "sonnet").as_deref(), Some("xhigh"));
}

#[test]
fn a_swap_keeps_a_level_the_user_changed_in_the_session() {
    let mut runtime = serde_json::json!({"modelSettings": {"opus": {"effortLevel": "medium"}}});
    let fresh = serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}});
    let outgoing = BTreeMap::from([("opus".to_string(), "high".to_string())]);
    hand_over(
        runtime.as_object_mut().unwrap(),
        fresh.as_object().unwrap(),
        &BTreeMap::new(),
        Some(&outgoing),
    )
    .unwrap();
    assert_eq!(level(&runtime, "opus").as_deref(), Some("medium"));
}

#[test]
fn a_swap_writes_the_carrier_and_the_members_levels_into_the_session() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    let runtime = crate::profile::clauth_dir()
        .unwrap()
        .join("session-settings.json");
    atomic_write(
        &runtime,
        r#"{"env":{"CLAUDE_CODE_EFFORT_LEVEL":""},"modelSettings":{"opus":{"effortLevel":"high"}}}"#,
    )
    .unwrap();
    let mut a = profile("a", Some("opus"), Some("high"), &[]);
    RuntimeLevels::write(&runtime, resolved(&a), BTreeMap::new()).unwrap();
    a.effort = Some("medium".into());
    assert_eq!(selected_level(&a).as_deref(), Some("medium"));
    let b = profile("b", Some("sonnet"), None, &[("sonnet", "xhigh")]);
    crate::claude::set_settings_effort(
        &runtime,
        None,
        "CLAUDE_CODE_EFFORT_LEVEL",
        &crate::claude::live_effort(&b),
        Some((&resolved(&b), true)),
    )
    .unwrap();
    let after: serde_json::Value = read_json_file(&runtime).unwrap();
    assert_eq!(level(&after, "opus").as_deref(), Some("low"));
    assert_eq!(level(&after, "sonnet").as_deref(), Some("xhigh"));
    assert_eq!(after["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "xhigh");
}

#[test]
fn a_swap_still_hands_over_the_carrier_when_the_live_settings_do_not_read() {
    let _home = HomeSandbox::new();
    atomic_write(&claude_settings_path().unwrap(), "{torn").unwrap();
    let runtime = crate::profile::clauth_dir()
        .unwrap()
        .join("session-settings.json");
    atomic_write(&runtime, r#"{"env":{"CLAUDE_CODE_EFFORT_LEVEL":"max"}}"#).unwrap();
    let b = profile("b", None, None, &[("sonnet", "xhigh")]);
    crate::claude::set_settings_effort(
        &runtime,
        None,
        "CLAUDE_CODE_EFFORT_LEVEL",
        "",
        Some((&resolved(&b), true)),
    )
    .expect("the carrier lands whatever the live file holds");
    let after: serde_json::Value = read_json_file(&runtime).unwrap();
    assert_eq!(after["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "");
}

#[test]
fn two_takeovers_whose_live_writes_failed_never_lose_the_users_level() {
    let home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"opus": {"effortLevel": "low"}}}));
    apply_profile_to_claude_settings(&profile("a", Some("opus"), Some("high"), &[]), &[]).unwrap();
    for (name, level) in [("b", "medium"), ("c", "xhigh")] {
        let _fault = FailLiveSettingsWrite::new(&home);
        apply_profile_to_claude_settings(&profile(name, Some("opus"), Some(level), &[]), &[])
            .expect_err("the live write fails after the record's pre-write");
    }
    assert_eq!(
        level(&live(), "opus").as_deref(),
        Some("high"),
        "a's level is still live"
    );
    switch_off();
    assert_eq!(level(&live(), "opus").as_deref(), Some("low"));
}

#[test]
fn an_alias_pin_holds_every_claude_id_of_its_tier_in_the_sync() {
    let pinned = std::collections::BTreeSet::from(["sonnet".to_string()]);
    assert!(is_pinned(&pinned, "sonnet"));
    assert!(is_pinned(&pinned, "claude-sonnet-4-5"));
    assert!(is_pinned(&pinned, "claude-sonnet-4-5-20250929[1m]"));
    assert!(!is_pinned(&pinned, "claude-opus-4-5"));
    assert!(!is_pinned(&pinned, "deepseek-sonnet"));
}

#[test]
fn the_effort_row_shows_the_selected_models_spelling_first() {
    let mut p = profile("p", Some("sonnet"), None, &[("claude-sonnet-4-5", "high")]);
    p.models.sonnet = Some("claude-sonnet-4-5".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("high"));
    assert_eq!(
        selected_entries(&p, "sonnet"),
        vec!["claude-sonnet-4-5".to_string()]
    );
    p.model_effort
        .insert("sonnet".to_string(), "low".to_string());
    assert_eq!(
        row_level(&p).as_deref(),
        Some("low"),
        "the row's own spelling first"
    );
    assert_eq!(
        resolved(&p).get("claude-sonnet-4-5").map(String::as_str),
        Some("low"),
        "a launch runs the level the row shows"
    );
}

#[test]
fn the_row_and_a_launch_agree_on_which_equivalent_entry_wins() {
    // The selected spelling is the full id: it wins over the alias, which a
    // plain table walk would visit last.
    let mut p = profile(
        "p",
        Some("claude-sonnet-4-5"),
        None,
        &[("claude-sonnet-4-5", "high"), ("sonnet", "low")],
    );
    p.models.sonnet = Some("claude-sonnet-4-5".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("high"));
    assert_eq!(
        resolved(&p).get("claude-sonnet-4-5").map(String::as_str),
        Some("high")
    );

    // Neither entry is the selected spelling: the table's first one wins.
    let mut p = profile(
        "p",
        Some("sonnet"),
        None,
        &[("claude-sonnet-4-5", "high"), ("claude-sonnet-4.5", "low")],
    );
    p.models.sonnet = Some("claude-sonnet-4-5".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("high"));
    assert_eq!(
        resolved(&p).get("claude-sonnet-4-5").map(String::as_str),
        Some("high")
    );

    // A value Claude Code drops never wins; the row shows the one that lands.
    let mut p = profile(
        "p",
        Some("sonnet"),
        None,
        &[("claude-sonnet-4-5", "max"), ("sonnet", "medium")],
    );
    p.models.sonnet = Some("claude-sonnet-4-5".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("medium"));
    assert_eq!(
        resolved(&p).get("claude-sonnet-4-5").map(String::as_str),
        Some("medium")
    );
}

#[test]
fn the_row_shows_the_dropped_entry_while_a_launch_uses_the_scalar() {
    let mut p = profile(
        "p",
        Some("sonnet"),
        Some("high"),
        &[("claude-sonnet-4-5", "max")],
    );
    p.models.sonnet = Some("claude-sonnet-4-5".to_string());
    assert_eq!(
        resolved(&p).get("claude-sonnet-4-5").map(String::as_str),
        Some("high")
    );
    assert_eq!(row_level(&p).as_deref(), Some("max"));
    assert!(invalid_selected_entry(&p));

    // With no level landing at all, the dropped value still shows.
    p.effort = None;
    assert_eq!(row_level(&p).as_deref(), Some("max"));
}

#[test]
fn a_table_entry_for_the_selected_model_beats_the_scalar_under_any_spelling() {
    let mut p = profile(
        "p",
        Some("sonnet"),
        Some("high"),
        &[("claude-sonnet-4-5", "low")],
    );
    p.models.sonnet = Some("claude-sonnet-4-5".to_string());
    assert_eq!(
        resolved(&p),
        BTreeMap::from([("claude-sonnet-4-5".to_string(), "low".to_string())])
    );
    assert_eq!(row_level(&p).as_deref(), Some("low"));
}

#[test]
fn an_invalid_table_entry_is_visible_and_the_valid_scalar_still_applies() {
    let p = profile("p", Some("opus"), Some("high"), &[("opus", "max")]);
    assert_eq!(row_level(&p).as_deref(), Some("max"));
    assert!(invalid_selected_entry(&p));
    assert_eq!(selected_level(&p).as_deref(), Some("high"));
}

#[test]
fn a_tier_covers_and_restores_an_existing_exact_key() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings":{"claude-opus-4-5":{"effortLevel":"medium"}}}));
    let p = profile("alias", Some("opus"), Some("high"), &[]);
    apply_profile_to_claude_settings(&p, &[]).unwrap();
    assert_eq!(level(&live(), "claude-opus-4-5").as_deref(), Some("high"));
    assert_eq!(live()["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "high");
    switch_off();
    assert_eq!(level(&live(), "claude-opus-4-5").as_deref(), Some("medium"));
    assert_eq!(live()["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "");
}

#[test]
fn runtime_build_handles_missing_malformed_and_versioned_model_settings() {
    let _home = HomeSandbox::new();
    let path = claude_settings_path().unwrap();
    let record = crate::claude::saved_base_env_path().unwrap();
    let p = profile("p", Some("opus"), Some("high"), &[]);
    atomic_write_600(
        &record,
        r#"{"v":2,"keys":{},"model_effort":{"opus":{"wrote":"old"}}}"#,
    )
    .unwrap();
    seed_live(serde_json::json!({}));
    let built: serde_json::Value =
        serde_json::from_str(&build_claude_settings_json(Some(&path), &p, &[]).unwrap()).unwrap();
    assert_eq!(built["modelSettings"]["opus"]["effortLevel"], "high");

    seed_live(serde_json::json!({"modelSettings": false}));
    let error = build_claude_settings_json(Some(&path), &p, &[]).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("fix or remove modelSettings in {}", path.display())
    );
    let error = apply_profile_to_claude_settings(&p, &[]).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("fix or remove modelSettings in {}", path.display())
    );

    atomic_write_600(&record, r#"{"v":99,"keys":{}}"#).unwrap();
    seed_live(serde_json::json!({"modelSettings": {}}));
    assert!(
        build_claude_settings_json(Some(&path), &p, &[])
            .unwrap_err()
            .to_string()
            .contains("base-env.json")
    );
}

#[test]
fn model_effort_warnings_name_the_invalid_entry_and_account_override() {
    let _home = HomeSandbox::new();
    let logs = crate::logline::LogLines::new();
    let _capture = logs.capture_here();
    let p = profile(
        "effort-warnings",
        None,
        Some("max"),
        &[("opus", "max"), ("sonnet", "low")],
    );
    build_claude_settings_json(None, &p, &[]).unwrap();
    build_claude_settings_json(None, &p, &[]).unwrap();
    assert_eq!(
        logs.snapshot(),
        [
            "clauth: profile 'effort-warnings': [model_effort] opus skipped: Claude Code keeps only low, medium, high, xhigh per model",
            "clauth: profile 'effort-warnings': the account-wide effort overrides its per-model levels",
        ]
    );
}

#[test]
fn a_v1_record_is_upgraded_before_covering_a_model_effort_level() {
    let home = HomeSandbox::new();
    let record = crate::claude::saved_base_env_path().unwrap();
    atomic_write_600(&record, r#"{"v":1,"keys":{}}"#).unwrap();
    seed_live(serde_json::json!({"modelSettings":{"opus":{"effortLevel":"low"}}}));
    let p = profile("p", Some("opus"), Some("high"), &[]);
    {
        let _fail = crate::claude::FailBaseEnvSave::new(&home);
        apply_profile_to_claude_settings(&p, &[]).unwrap_err();
        assert_eq!(live()["modelSettings"]["opus"]["effortLevel"], "low");
    }
    apply_profile_to_claude_settings(&p, &[]).unwrap();
    let saved: serde_json::Value = read_json_file(&record).unwrap();
    assert_eq!(saved["v"], 2);
    assert_eq!(saved["model_effort"]["opus"]["base"], "low");
    switch_off();
    assert_eq!(live()["modelSettings"]["opus"]["effortLevel"], "low");
}

/// Persist `p` as the one configured account, so the Setup edit reads it off
/// disk.
fn saved(p: Profile) -> crate::profile::AppConfig {
    let profiles = vec![p];
    crate::testutil::persist_profiles(&profiles);
    crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles,
    }
}

/// Run the Setup tab's effort edit on `name`, setting its row to `value`.
fn edit_row(config: &mut crate::profile::AppConfig, name: &str, value: Option<&str>) -> Profile {
    let name = crate::profile::ProfileName::from(name);
    crate::actions::edit_profile_effort(config, &name, |slot| *slot = value.map(str::to_string))
        .expect("edit the effort row");
    crate::profile::load_profile(&name).expect("reload")
}

fn table(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A dated `[1m]` id under the `opus` row, a level under the alias and another
/// under the model's canonical name.
fn dated_opus_row(name: &str) -> Profile {
    let mut p = profile(
        name,
        Some("opus"),
        None,
        &[("opus", "xhigh"), ("claude-opus-4-5", "high")],
    );
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    p
}

/// An account-wide `effort` no per-model level can hold, over a level for the
/// selected model.
fn account_wide_over_a_level(name: &str) -> Profile {
    let mut p = profile(
        name,
        Some("opus"),
        Some("max"),
        &[("claude-opus-4-5", "low")],
    );
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    p
}

/// An account-wide `effort` no per-model level can hold, with nothing in the
/// table for the selected model.
fn account_wide_with_no_level(name: &str) -> Profile {
    let mut p = profile(name, Some("opus"), Some("max"), &[]);
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    p
}

/// The `sonnet` row's `@date` id under the alias, and the same model's `-date`
/// id typed out, which sorts ahead of it in the built file.
fn two_dated_sonnet_spellings() -> Profile {
    let mut p = profile(
        "p",
        Some("sonnet"),
        None,
        &[("sonnet", "low"), ("claude-sonnet-4-5-20250929", "high")],
    );
    p.models.sonnet = Some("claude-sonnet-4-5@20250929".to_string());
    p
}

#[test]
fn canonical_name_folds_each_spelling_claude_code_folds() {
    for (id, name) in [
        ("claude-opus-4-5", "claude-opus-4-5"),
        ("claude-opus-4-5-20251101[1m]", "claude-opus-4-5"),
        ("Claude-Opus-4.5", "claude-opus-4-5"),
        ("claude-sonnet-4-5@20250929", "claude-sonnet-4-5"),
        (
            "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "claude-opus-4-5",
        ),
        (
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "claude-haiku-4-5",
        ),
        ("claude-opus-4-20250514", "claude-opus-4-0"),
        ("claude-opus-5-0", "claude-opus-5"),
        ("claude-3-5-sonnet-20241022", "claude-3-5-sonnet"),
        ("claude-opus-4-6-fast", "claude-opus-4-6"),
        ("deepseek-v4-pro", "deepseek-v4-pro"),
        ("glm-5-20260101", "glm-5"),
        ("opus", "opus"),
    ] {
        assert_eq!(canonical_name(id), name, "{id}");
    }
}

#[test]
fn a_pinned_id_holds_only_the_keys_naming_its_own_model() {
    let pinned = std::collections::BTreeSet::from(["claude-opus-4-5-20251101[1m]".to_string()]);
    assert!(is_pinned(&pinned, "claude-opus-4-5"), "its canonical key");
    assert!(
        is_pinned(&pinned, "us.anthropic.claude-opus-4-5-20251101-v1:0"),
        "a provider-prefixed spelling of it"
    );
    assert!(
        !is_pinned(&pinned, "claude-opus-5-5"),
        "another model of its tier"
    );
    assert!(!is_pinned(&pinned, "claude-sonnet-4-5"), "another tier");
    assert!(
        !is_pinned(&pinned, "opus"),
        "the alias resolves per session"
    );
}

#[test]
fn an_alias_pin_holds_every_id_of_its_tier_in_either_shape() {
    let pinned = std::collections::BTreeSet::from(["opus".to_string()]);
    assert!(is_pinned(
        &pinned,
        "eu.anthropic.claude-opus-4-5-20251101-v1:0"
    ));
    assert!(
        is_pinned(&pinned, "claude-3-opus"),
        "the older family-last id of its tier"
    );
    assert!(!is_pinned(
        &pinned,
        "eu.anthropic.claude-sonnet-4-5-20250929-v1:0"
    ));
    assert!(
        !is_pinned(&pinned, "claude-3-5-sonnet"),
        "a family-last id of another tier"
    );
    let sonnet = std::collections::BTreeSet::from(["sonnet".to_string()]);
    for id in [
        "claude-3-5-sonnet",
        "claude-3-7-sonnet",
        "claude-sonnet-4-5",
    ] {
        assert!(is_pinned(&sonnet, id), "{id}");
    }
    assert!(!is_pinned(&sonnet, "claude-3-opus"), "another tier");
}

#[test]
fn a_models_row_id_covers_and_restores_the_models_canonical_key() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({"modelSettings": {"claude-opus-4-5": {"effortLevel": "medium"}}}));
    let mut p = profile("pinned", Some("opus"), Some("high"), &[]);
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    let built: serde_json::Value = serde_json::from_str(
        &build_claude_settings_json(Some(&claude_settings_path().unwrap()), &p, &[]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        level(&built, "claude-opus-4-5").as_deref(),
        Some("high"),
        "a session reads the canonical key first"
    );

    apply_profile_to_claude_settings(&p, &[]).unwrap();
    assert_eq!(level(&live(), "claude-opus-4-5").as_deref(), Some("high"));
    assert_eq!(
        level(&live(), "claude-opus-4-5-20251101[1m]").as_deref(),
        Some("high")
    );
    switch_off();
    assert_eq!(
        level(&live(), "claude-opus-4-5").as_deref(),
        Some("medium"),
        "the user's level comes back"
    );

    let typed = profile("typed", None, None, &[("claude-opus-4-5-20251101", "low")]);
    let built: serde_json::Value = serde_json::from_str(
        &build_claude_settings_json(Some(&claude_settings_path().unwrap()), &typed, &[]).unwrap(),
    )
    .unwrap();
    assert_eq!(level(&built, "claude-opus-4-5").as_deref(), Some("low"));
}

/// A spelling of the model's canonical name already in the live file, not only
/// the canonical spelling itself, takes the account's level.
#[test]
fn an_existing_spelling_of_the_models_canonical_name_is_covered() {
    let _home = HomeSandbox::new();
    seed_live(serde_json::json!({
        "modelSettings": {
            "eu.anthropic.claude-opus-4-5-20251101-v1:0": {"effortLevel": "medium"}
        }
    }));
    let mut p = profile("pinned", Some("opus"), Some("high"), &[]);
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    let built: serde_json::Value = serde_json::from_str(
        &build_claude_settings_json(Some(&claude_settings_path().unwrap()), &p, &[]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        level(&built, "eu.anthropic.claude-opus-4-5-20251101-v1:0").as_deref(),
        Some("high"),
        "the existing spelling folds to the account's model"
    );
}

#[test]
fn a_selected_alias_with_no_models_row_carries_only_its_own_entry() {
    let p = profile(
        "p",
        Some("opus"),
        None,
        &[("opus", "max"), ("claude-opus-4-5", "high")],
    );
    assert_eq!(
        live_effort(&p),
        "",
        "an id of the tier is no level of the alias, which runs another version"
    );
    let p = profile(
        "p",
        Some("opus"),
        None,
        &[("opus", "low"), ("claude-opus-4-5", "high")],
    );
    assert_eq!(live_effort(&p), "low");
}

#[test]
fn the_row_shows_edits_and_clears_the_entry_a_launch_applies() {
    let _home = HomeSandbox::new();
    let p = dated_opus_row("fa");
    assert_eq!(
        row_level(&p).as_deref(),
        Some("high"),
        "the exact canonical key is the one Claude Code reads"
    );
    let mut config = saved(p);
    let after = edit_row(&mut config, "fa", Some("low"));
    assert_eq!(
        after.model_effort,
        table(&[("claude-opus-4-5", "low"), ("opus", "xhigh")])
    );
    let after = edit_row(&mut config, "fa", None);
    assert_eq!(after.model_effort, table(&[("opus", "xhigh")]));
    assert_eq!(
        row_level(&after).as_deref(),
        Some("xhigh"),
        "the level a launch applies once the canonical key is gone"
    );
}

#[test]
fn the_row_never_shows_a_level_no_launch_applies() {
    let _home = HomeSandbox::new();
    let p = profile("fb", Some("opus"), None, &[("claude-opus-4-5", "high")]);
    assert_eq!(row_level(&p), None);
    let mut config = saved(p);
    let after = edit_row(&mut config, "fb", Some("low"));
    assert_eq!(
        after.model_effort,
        table(&[("claude-opus-4-5", "high"), ("opus", "low")])
    );
    assert_eq!(row_level(&after).as_deref(), Some("low"));
    let after = edit_row(&mut config, "fb", None);
    assert_eq!(after.model_effort, table(&[("claude-opus-4-5", "high")]));
    assert_eq!(row_level(&after), None);
}

#[test]
fn the_row_shows_the_account_wide_effort_no_model_level_can_hold() {
    for scalar in ["max", "med", "8000"] {
        let mut over_a_level = account_wide_over_a_level("p");
        over_a_level.effort = Some(scalar.to_string());
        let mut no_level = account_wide_with_no_level("p");
        no_level.effort = Some(scalar.to_string());
        for p in [over_a_level, no_level] {
            assert_eq!(row_level(&p).as_deref(), Some(scalar));
            assert_eq!(live_effort(&p), scalar, "the carrier applies it");
            assert_eq!(selected_level(&p).as_deref(), Some(scalar));
            assert!(
                !invalid_selected_entry(&p),
                "an account-wide effort is no dropped entry"
            );
        }
    }

    // A value the carrier's parser refuses applies nothing: the model's entry
    // is what a launch applies, and the carrier hands over the value Claude
    // Code falls back from.
    let mut refused = account_wide_over_a_level("p");
    refused.effort = Some("turbo".to_string());
    assert_eq!(row_level(&refused).as_deref(), Some("low"));
    assert_eq!(selected_level(&refused).as_deref(), Some("low"));
    assert_eq!(
        live_effort(&refused),
        "low",
        "a dropped value hands the level the launch applies"
    );

    // An entry Claude Code drops for the model is not what the row shows, so
    // the row does not mark it.
    let mut dropped = account_wide_with_no_level("p");
    dropped
        .model_effort
        .insert("opus".to_string(), "turbo".to_string());
    assert_eq!(row_level(&dropped).as_deref(), Some("max"));
    assert!(!invalid_selected_entry(&dropped));

    // An empty scalar is no account-wide value: the model's own entry shows.
    let mut empty = account_wide_over_a_level("p");
    empty.effort = Some(String::new());
    assert_eq!(row_level(&empty).as_deref(), Some("low"));
    assert_eq!(live_effort(&empty), "low");
}

#[test]
fn an_env_override_is_the_effort_the_row_shows() {
    let mut p = account_wide_over_a_level("p");
    p.env
        .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "high".to_string());
    assert_eq!(
        row_level(&p).as_deref(),
        Some("high"),
        "the carrier is the env entry, so the launch applies its level"
    );
    assert_eq!(live_effort(&p), "high");
    assert!(!invalid_selected_entry(&p));

    // A value the parser drops leaves the model's own level in force.
    p.env
        .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "turbo".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("low"));
    assert_eq!(live_effort(&p), "low");
}

#[test]
fn an_edit_from_the_account_wide_effort_writes_the_models_entry_and_clears_it() {
    let _home = HomeSandbox::new();
    let over_a_level = account_wide_over_a_level("p");
    assert_eq!(row_level(&over_a_level).as_deref(), Some("max"));
    let mut config = saved(over_a_level);
    let after = edit_row(&mut config, "p", Some("medium"));
    assert_eq!(after.model_effort, table(&[("claude-opus-4-5", "medium")]));
    assert_eq!(after.effort, None, "the account-wide value leaves");
    assert_eq!(row_level(&after).as_deref(), Some("medium"));
    assert_eq!(live_effort(&after), "medium");

    let no_level = account_wide_with_no_level("p");
    assert_eq!(row_level(&no_level).as_deref(), Some("max"));
    let mut config = saved(no_level);
    let after = edit_row(&mut config, "p", Some("medium"));
    assert_eq!(after.model_effort, table(&[("opus", "medium")]));
    assert_eq!(after.effort, None);
    assert_eq!(row_level(&after).as_deref(), Some("medium"));
    assert_eq!(live_effort(&after), "medium");
}

#[test]
fn a_clear_of_the_account_wide_effort_leaves_the_models_own_entry() {
    let _home = HomeSandbox::new();
    let mut config = saved(account_wide_over_a_level("p"));
    let after = edit_row(&mut config, "p", None);
    assert_eq!(
        after.model_effort,
        table(&[("claude-opus-4-5", "low")]),
        "the level the launch applies after the clear stays"
    );
    assert_eq!(after.effort, None);
    assert_eq!(row_level(&after).as_deref(), Some("low"));
    assert_eq!(live_effort(&after), "low");

    let mut config = saved(account_wide_with_no_level("p"));
    let after = edit_row(&mut config, "p", None);
    assert!(after.model_effort.is_empty());
    assert_eq!(after.effort, None);
    assert_eq!(row_level(&after), None);
    assert_eq!(live_effort(&after), "");
}

/// The resolver Claude Code 2.1.330 runs over the carrier and the settings
/// table, mirrored: every account-wide value class, each over a per-model
/// entry and with none. The Setup row and the value a moved session's carrier
/// receives both show what a fresh launch applies; an edit writes the entry
/// and clears the account-wide value, and a clear removes that value alone.
#[test]
fn the_row_and_the_carrier_show_what_a_launch_applies_on_every_effort_class() {
    let _home = HomeSandbox::new();
    let want = |value: &str| (!value.is_empty()).then(|| value.to_string());
    let fixtures = |name: &str| {
        [
            ("an entry", account_wide_over_a_level(name)),
            ("no entry", account_wide_with_no_level(name)),
        ]
    };
    let assert_case = |case: &str, p: &Profile, expected: &str| {
        assert_eq!(row_level(p), want(expected), "{case}: row");
        assert_eq!(live_effort(p), expected, "{case}: carrier");
        assert_eq!(selected_level(p), want(expected), "{case}: decided level");
        assert!(!invalid_selected_entry(p), "{case}: no dropped entry");
    };

    // A scalar `effort`: a per-model level is that model's level (the carrier
    // stays empty), a value the parser takes rides the carrier, `auto`/`unset`
    // run the model default, and a value it drops leaves the per-model level
    // (or nothing) in force.
    for (scalar, with_entry, without) in [
        ("low", "low", "low"),
        ("medium", "low", "medium"),
        ("high", "low", "high"),
        ("xhigh", "low", "xhigh"),
        ("max", "max", "max"),
        ("med", "med", "med"),
        ("8000", "8000", "8000"),
        ("+8000", "+8000", "+8000"),
        (" 8000", " 8000", " 8000"),
        ("auto", "auto", "auto"),
        ("unset", "auto", "auto"),
        ("turbo", "low", ""),
        ("max ", "low", ""),
        ("", "low", ""),
    ] {
        for (label, mut p) in fixtures("p") {
            p.effort = Some(scalar.to_string());
            let expected = if label == "an entry" {
                with_entry
            } else {
                without
            };
            assert_case(&format!("{scalar:?}, {label}"), &p, expected);
        }
    }

    // A settings `maxEffortLevel` cap changes neither the row nor the carrier:
    // `auto`/`unset` run the model default, capped, so wherever the cap sits
    // the row shows `auto` and the carrier hands `auto`; `max` itself is no cap.
    for settings in [
        serde_json::json!({"maxEffortLevel": "high"}),
        serde_json::json!({"modelSettings": {"claude-opus-4-5": {"maxEffortLevel": "medium"}}}),
        serde_json::json!({"maxEffortLevel": "max"}),
    ] {
        seed_live(settings.clone());
        for scalar in ["auto", "unset"] {
            for (label, mut p) in fixtures("p") {
                p.effort = Some(scalar.to_string());
                assert_case(&format!("{scalar:?} under {settings}, {label}"), &p, "auto");
                assert!(
                    selected_effort(&p).expect("a selected model").account_wide,
                    "{scalar:?} under {settings}, {label}: a carrier value"
                );
            }
        }
    }
    // The `[env]` value goes through the same parser, cap or no cap.
    for settings in [
        serde_json::json!({}),
        serde_json::json!({"maxEffortLevel": "high"}),
    ] {
        seed_live(settings.clone());
        for env in ["auto", "unset"] {
            for (label, mut p) in fixtures("p") {
                p.effort = Some("max".to_string());
                p.env
                    .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), env.to_string());
                assert_case(
                    &format!("[env] {env:?} under {settings}, {label}"),
                    &p,
                    "auto",
                );
                assert!(
                    selected_effort(&p).expect("a selected model").account_wide,
                    "[env] {env:?} under {settings}, {label}: a carrier value"
                );
            }
        }
    }
    seed_live(serde_json::json!({}));

    // An `[env]` override is the carrier a launch builds with: the row and the
    // carried value follow it, not the account's own `effort`.
    for (env, with_entry, without) in [
        ("high", "high", "high"),
        ("med", "med", "med"),
        ("8000", "8000", "8000"),
        ("auto", "auto", "auto"),
        ("unset", "auto", "auto"),
        ("turbo", "low", ""),
        ("", "low", ""),
    ] {
        for (label, mut p) in fixtures("p") {
            p.effort = Some("max".to_string());
            p.env
                .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), env.to_string());
            let expected = if label == "an entry" {
                with_entry
            } else {
                without
            };
            assert_case(&format!("[env] {env:?}, {label}"), &p, expected);
        }
    }

    // The edit target and the clear rule on every class the row shows as the
    // account's own `effort`.
    for scalar in ["max", "med", "8000", "+8000", " 8000", "auto", "unset"] {
        for (label, mut p) in fixtures("p") {
            p.effort = Some(scalar.to_string());
            let target = if label == "an entry" {
                "claude-opus-4-5"
            } else {
                "opus"
            };
            let mut config = saved(p.clone());
            let after = edit_row(&mut config, "p", Some("medium"));
            assert_eq!(
                after.model_effort.get(target).map(String::as_str),
                Some("medium"),
                "{scalar:?}, {label}: the edit writes the entry"
            );
            assert_eq!(
                after.effort, None,
                "{scalar:?}, {label}: the edit clears the account-wide value"
            );
            assert_eq!(
                row_level(&after).as_deref(),
                Some("medium"),
                "{scalar:?}, {label}: the edited level applies"
            );

            let mut config = saved(p);
            let after = edit_row(&mut config, "p", None);
            assert_eq!(
                after.effort, None,
                "{scalar:?}, {label}: the clear removes the account-wide value"
            );
            if label == "an entry" {
                assert_eq!(
                    after
                        .model_effort
                        .get("claude-opus-4-5")
                        .map(String::as_str),
                    Some("low"),
                    "{scalar:?}: the entry the value overrode stays"
                );
                assert_eq!(
                    row_level(&after).as_deref(),
                    Some("low"),
                    "{scalar:?}: the entry applies again"
                );
            } else {
                assert!(after.model_effort.is_empty(), "{scalar:?}");
                assert_eq!(row_level(&after), None, "{scalar:?}: nothing applies");
            }
        }
    }

    // The edit and clear rules hold under a cap too: the row shows a carrier
    // value, so an edit writes the entry and removes `effort`, and a clear
    // removes `effort` alone, leaving the entry it overrode.
    for settings in [
        serde_json::json!({"maxEffortLevel": "high"}),
        serde_json::json!({"modelSettings": {"claude-opus-4-5": {"maxEffortLevel": "medium"}}}),
    ] {
        seed_live(settings);
        for scalar in ["auto", "unset"] {
            for (label, mut p) in fixtures("p") {
                p.effort = Some(scalar.to_string());
                let target = if label == "an entry" {
                    "claude-opus-4-5"
                } else {
                    "opus"
                };
                let mut config = saved(p.clone());
                let after = edit_row(&mut config, "p", Some("medium"));
                assert_eq!(
                    after.model_effort.get(target).map(String::as_str),
                    Some("medium"),
                    "{scalar:?} under a cap, {label}: the edit writes the entry"
                );
                assert_eq!(
                    after.effort, None,
                    "{scalar:?} under a cap, {label}: the edit clears the account-wide value"
                );
                assert_eq!(
                    row_level(&after).as_deref(),
                    Some("medium"),
                    "{scalar:?} under a cap, {label}: the edited level applies"
                );

                let mut config = saved(p);
                let after = edit_row(&mut config, "p", None);
                assert_eq!(
                    after.effort, None,
                    "{scalar:?} under a cap, {label}: the clear removes the account-wide value"
                );
                if label == "an entry" {
                    assert_eq!(
                        after
                            .model_effort
                            .get("claude-opus-4-5")
                            .map(String::as_str),
                        Some("low"),
                        "{scalar:?} under a cap: the entry the value overrode stays"
                    );
                    assert_eq!(row_level(&after).as_deref(), Some("low"));
                } else {
                    assert!(after.model_effort.is_empty(), "{scalar:?} under a cap");
                    assert_eq!(row_level(&after), None, "{scalar:?} under a cap");
                }
            }
        }
    }
    seed_live(serde_json::json!({}));

    // An `[env]` carrier value is account-wide too, cap or no cap: an edit
    // writes the entry and removes the account's `effort`, and a clear removes
    // that `effort` alone, leaving the entry it overrode and the `[env]` entry
    // in place.
    for settings in [
        serde_json::json!({}),
        serde_json::json!({"maxEffortLevel": "high"}),
    ] {
        seed_live(settings.clone());
        for env in ["high", "med", "8000", "auto", "unset"] {
            let mut p = account_wide_over_a_level("p");
            p.effort = Some(env.to_string());
            p.env
                .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), env.to_string());
            let mut config = saved(p.clone());
            let after = edit_row(&mut config, "p", Some("medium"));
            assert_eq!(
                after
                    .model_effort
                    .get("claude-opus-4-5")
                    .map(String::as_str),
                Some("medium"),
                "[env] {env:?} under {settings}: the edit writes the entry"
            );
            assert_eq!(
                after.effort, None,
                "[env] {env:?} under {settings}: the edit removes the account's `effort`"
            );
            assert_eq!(
                after
                    .env
                    .get("CLAUDE_CODE_EFFORT_LEVEL")
                    .map(String::as_str),
                Some(env),
                "[env] {env:?} under {settings}: the edit leaves the [env] value"
            );
            assert_eq!(
                row_level(&after).as_deref(),
                Some(if env == "auto" || env == "unset" {
                    "auto"
                } else {
                    env
                }),
                "[env] {env:?} under {settings}: the [env] value still applies"
            );

            let mut config = saved(p);
            let after = edit_row(&mut config, "p", None);
            assert_eq!(
                after.model_effort,
                table(&[("claude-opus-4-5", "low")]),
                "[env] {env:?} under {settings}: the clear leaves the entry the value overrode"
            );
            assert_eq!(
                after.effort, None,
                "[env] {env:?} under {settings}: the clear removes the account's `effort`"
            );
            assert_eq!(
                after
                    .env
                    .get("CLAUDE_CODE_EFFORT_LEVEL")
                    .map(String::as_str),
                Some(env),
                "[env] {env:?} under {settings}: the clear leaves the [env] value as it was"
            );
        }
    }
    seed_live(serde_json::json!({}));

    // An `[env]` carrier value is account-wide too: the clear removes the
    // account's `effort` alone, leaving the entry it overrode and the `[env]`
    // entry in place.
    for env in ["high", "med", "8000", "auto", "unset"] {
        let mut p = account_wide_over_a_level("p");
        p.effort = None;
        p.env
            .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), env.to_string());
        let mut config = saved(p);
        let after = edit_row(&mut config, "p", None);
        assert_eq!(
            after.model_effort,
            table(&[("claude-opus-4-5", "low")]),
            "[env] {env:?}: the clear leaves the entry the value overrode"
        );
        assert_eq!(
            after
                .env
                .get("CLAUDE_CODE_EFFORT_LEVEL")
                .map(String::as_str),
            Some(env),
            "[env] {env:?}: the clear leaves the [env] value as it was"
        );
    }
}

#[test]
fn two_spellings_of_the_selected_model_apply_the_first_one_the_built_file_holds() {
    let p = two_dated_sonnet_spellings();
    let built: serde_json::Value =
        serde_json::from_str(&build_claude_settings_json(None, &p, &[]).unwrap()).unwrap();
    let keys: Vec<&str> = built["modelSettings"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["claude-sonnet-4-5-20250929", "claude-sonnet-4-5@20250929"]
    );
    assert_eq!(row_level(&p).as_deref(), Some("high"));
    assert_eq!(live_effort(&p), "high");
}

#[test]
fn an_exact_canonical_key_wins_wherever_it_sorts() {
    let mut p = profile(
        "p",
        Some("opus"),
        None,
        &[("Claude-Opus-4-5", "low"), ("claude-opus-4-5", "high")],
    );
    p.models.opus = Some("claude-opus-4-5-20251101".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("high"));
    assert_eq!(live_effort(&p), "high");
}

#[test]
fn a_provider_prefixed_models_row_lands_on_its_release() {
    let mut p = profile(
        "p",
        Some("opus"),
        None,
        &[("opus", "low"), ("claude-opus-4-5", "high")],
    );
    p.models.opus = Some("us.anthropic.claude-opus-4-5-20251101-v1:0".to_string());
    assert_eq!(row_level(&p).as_deref(), Some("high"));
    assert_eq!(live_effort(&p), "high");
}

/// Over every input of the selected-model checks, the carrier a moved session
/// takes is the level the row shows; a row showing an entry Claude Code drops
/// marks it, and the carrier then holds what applies instead.
#[test]
fn the_carrier_and_the_row_agree_on_every_selected_model_input() {
    let _home = HomeSandbox::new();
    let mut cleared = dated_opus_row("p");
    cleared.model_effort.remove("claude-opus-4-5");
    let mut edited = profile("p", Some("opus"), None, &[("claude-opus-4-5", "high")]);
    edited
        .model_effort
        .insert("opus".to_string(), "low".to_string());
    let cases: Vec<(&str, Profile, &str, bool)> = [
        (
            "dropped alias entry",
            profile(
                "p",
                Some("opus"),
                None,
                &[("opus", "max"), ("claude-opus-4-5", "high")],
            ),
            "",
            true,
        ),
        (
            "valid alias entry",
            profile(
                "p",
                Some("opus"),
                None,
                &[("opus", "low"), ("claude-opus-4-5", "high")],
            ),
            "low",
            false,
        ),
        (
            "canonical key over the row id",
            dated_opus_row("p"),
            "high",
            false,
        ),
        (
            "canonical key edited",
            {
                let mut p = dated_opus_row("p");
                p.model_effort
                    .insert("claude-opus-4-5".to_string(), "low".to_string());
                p
            },
            "low",
            false,
        ),
        ("canonical key cleared", cleared, "xhigh", false),
        (
            "an id of the alias's tier",
            profile("p", Some("opus"), None, &[("claude-opus-4-5", "high")]),
            "",
            false,
        ),
        ("the alias's own entry added", edited, "low", false),
        (
            "two spellings, file order",
            two_dated_sonnet_spellings(),
            "high",
            false,
        ),
        (
            "a mixed-case spelling first",
            {
                let mut p = profile(
                    "p",
                    Some("opus"),
                    None,
                    &[("Claude-Opus-4-5", "low"), ("claude-opus-4-5", "high")],
                );
                p.models.opus = Some("claude-opus-4-5-20251101".to_string());
                p
            },
            "high",
            false,
        ),
        (
            "an account-wide effort over a model level",
            account_wide_over_a_level("p"),
            "max",
            false,
        ),
        (
            "an account-wide effort with no model level",
            account_wide_with_no_level("p"),
            "max",
            false,
        ),
        (
            "an account-wide effort beside an entry Claude Code drops",
            {
                let mut p = account_wide_with_no_level("p");
                p.model_effort
                    .insert("opus".to_string(), "turbo".to_string());
                p
            },
            "max",
            false,
        ),
        (
            "the account-wide effort edited to a level",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = None;
                p.model_effort
                    .insert("claude-opus-4-5".to_string(), "medium".to_string());
                p
            },
            "medium",
            false,
        ),
        (
            "the account-wide effort cleared",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = None;
                p
            },
            "low",
            false,
        ),
        (
            "the account-wide effort cleared with no model level",
            {
                let mut p = account_wide_with_no_level("p");
                p.effort = None;
                p
            },
            "",
            false,
        ),
    ]
    .into_iter()
    .chain([
        (
            "an auto account-wide effort",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some("auto".to_string());
                p
            },
            "auto",
            false,
        ),
        (
            "an auto effort with no model level",
            {
                let mut p = account_wide_with_no_level("p");
                p.effort = Some("auto".to_string());
                p
            },
            "auto",
            false,
        ),
        (
            "an unset account-wide effort",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some("unset".to_string());
                p
            },
            "auto",
            false,
        ),
        (
            "a dropped account-wide effort",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some("turbo".to_string());
                p
            },
            "low",
            false,
        ),
        (
            "a dropped effort with no model level",
            {
                let mut p = account_wide_with_no_level("p");
                p.effort = Some("turbo".to_string());
                p
            },
            "",
            false,
        ),
        (
            "a trailing-space effort",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some("max ".to_string());
                p
            },
            "low",
            false,
        ),
        (
            "an empty effort",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some(String::new());
                p
            },
            "low",
            false,
        ),
        (
            "a signed budget",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some("+8000".to_string());
                p
            },
            "+8000",
            false,
        ),
        (
            "a whitespace-prefixed budget",
            {
                let mut p = account_wide_over_a_level("p");
                p.effort = Some(" 8000".to_string());
                p
            },
            " 8000",
            false,
        ),
        (
            "an [env] level override",
            {
                let mut p = account_wide_over_a_level("p");
                p.env
                    .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "high".to_string());
                p
            },
            "high",
            false,
        ),
        (
            "an [env] budget override",
            {
                let mut p = account_wide_over_a_level("p");
                p.env
                    .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "8000".to_string());
                p
            },
            "8000",
            false,
        ),
        (
            "an empty [env] override",
            {
                let mut p = account_wide_over_a_level("p");
                p.env
                    .insert("CLAUDE_CODE_EFFORT_LEVEL".to_string(), String::new());
                p
            },
            "low",
            false,
        ),
    ])
    .collect();
    for (case, p, level, dropped) in cases {
        let carrier = live_effort(&p);
        assert_eq!(carrier, level, "{case}: carrier");
        assert_eq!(
            selected_level(&p).unwrap_or_default(),
            level,
            "{case}: the decided level"
        );
        assert_eq!(invalid_selected_entry(&p), dropped, "{case}: dropped");
        if dropped {
            assert_eq!(row_level(&p).as_deref(), Some("max"), "{case}: row");
        } else {
            assert_eq!(row_level(&p).unwrap_or_default(), carrier, "{case}: row");
        }
    }
}
