use super::*;

use crate::claude::{
    FailLiveSettingsWrite, apply_profile_to_claude_settings, build_claude_settings_json,
    claude_settings_path, effective_effort, release_saved_base_env,
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
fn canonical_model_id_folds_claude_spellings_and_leaves_other_ids() {
    for (raw, want) in [
        ("claude-opus-4-8", "claude-opus-4-8"),
        ("claude-opus-4-8[1m]", "claude-opus-4-8"),
        ("Claude-Sonnet-4-5[2m]", "claude-sonnet-4-5"),
        ("claude-opus-4-5-20251101", "claude-opus-4-5"),
        ("claude-opus-4-5@20251101", "claude-opus-4-5"),
        (
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "claude-sonnet-4-5",
        ),
        (
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            "claude-haiku-4-5",
        ),
        ("~anthropic/claude-opus-latest[1m]", "claude-opus-latest"),
        ("claude-sonnet-4.5", "claude-sonnet-4-5"),
        ("claude-sonnet-4-5-v1@20250929", "claude-sonnet-4-5"),
        ("claude-sonnet-4-5-latest", "claude-sonnet-4-5"),
        ("claude-sonnet-4-5-latest-20250929", "claude-sonnet-4-5"),
        ("claude-opus-4-6-fast[1m]", "claude-opus-4-6"),
        ("claude-3-5-sonnet-latest", "claude-3-5-sonnet"),
        ("deepseek-v4-pro[1m]", "deepseek-v4-pro"),
        ("deepseek-v4-pro-v1", "deepseek-v4-pro-v1"),
        ("MiniMax-M3", "MiniMax-M3"),
        ("opus", "opus"),
    ] {
        assert_eq!(canonical_model_id(raw), want, "{raw}");
    }
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
fn a_tier_with_a_models_row_resolves_to_its_canonical_id() {
    let mut p = profile("p", Some("opus"), Some("high"), &[("sonnet", "low")]);
    p.models.opus = Some("claude-opus-4-5-20251101[1m]".to_string());
    assert_eq!(
        resolved(&p),
        BTreeMap::from([
            ("claude-opus-4-5".to_string(), "high".to_string()),
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
    let record: Record = read_json_file(&record_path().unwrap()).unwrap();
    assert!(
        record.ids.is_empty(),
        "the cover ended, so its entry retired"
    );
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
    atomic_write_600(&record_path().unwrap(), "{not json").unwrap();
    let err = build_claude_settings_json(
        Some(&claude_settings_path().unwrap()),
        &profile("b", None, None, &[]),
        &[],
    )
    .expect_err("a torn record could hide another account's level");
    assert!(format!("{err:#}").contains("model-effort.json"), "{err:#}");
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
    let a = profile("a", Some("opus"), Some("high"), &[]);
    let b = profile("b", None, None, &[("sonnet", "xhigh")]);
    crate::claude::set_settings_effort(
        &runtime,
        None,
        "CLAUDE_CODE_EFFORT_LEVEL",
        &effective_effort(&b),
        Some(Handover {
            incoming: &resolved(&b),
            outgoing: Some(&resolved(&a)),
        }),
    )
    .unwrap();
    let after: serde_json::Value = read_json_file(&runtime).unwrap();
    assert_eq!(level(&after, "opus").as_deref(), Some("low"));
    assert_eq!(level(&after, "sonnet").as_deref(), Some("xhigh"));
    assert_eq!(after["env"]["CLAUDE_CODE_EFFORT_LEVEL"], "");
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
        Some(Handover {
            incoming: &resolved(&b),
            outgoing: None,
        }),
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
fn the_effort_row_shows_and_replaces_every_spelling_of_the_selected_model() {
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
