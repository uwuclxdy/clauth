//! Regression tests pinning the serde alias that lets clauth 0.2.0 users
//! upgrade without losing their persisted settings: `kick_timer` (per-profile
//! config.toml) was renamed to `auto_start` after 0.2.0. Drop the alias and the
//! test below fails.

use super::*;

#[test]
fn profile_config_reads_kick_timer_as_auto_start() {
    let toml = "kick_timer = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse old config");
    assert!(cfg.auto_start);
}

#[test]
fn profile_config_reads_auto_start_directly() {
    let toml = "auto_start = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse new config");
    assert!(cfg.auto_start);
}

// Drop `bell_threshold` from `ProfileConfig` and the hand-edited value is
// silently ignored on load (the bug this pins): the field must round-trip.
#[test]
fn profile_config_reads_bell_threshold() {
    let toml = "bell_threshold = 90.0\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse bell config");
    assert_eq!(cfg.bell_threshold, Some(90.0));
}

// `last_resort` (issue #8 follow-up) must default to `false` so every existing
// config.toml written before this field existed keeps loading unchanged.
#[test]
fn profile_config_last_resort_defaults_false() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert!(!cfg.last_resort);
}

#[test]
fn profile_config_reads_last_resort_true() {
    let toml = "last_resort = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse last_resort config");
    assert!(cfg.last_resort);
}

// `last_resort` must survive a config.toml render→parse round-trip, matching
// the guarantee `model_settings_round_trip_through_config_toml` pins for models.
#[test]
fn last_resort_round_trips_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.last_resort = true;
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert!(parsed.last_resort);
}

// `disabled` (the per-account exclusion toggle) must default to `false` so
// every existing config.toml written before this field existed keeps loading
// unchanged, matching `last_resort`'s guarantee above.
#[test]
fn profile_config_disabled_defaults_false() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert!(!cfg.disabled);
}

#[test]
fn profile_config_reads_disabled_true() {
    let toml = "disabled = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse disabled config");
    assert!(cfg.disabled);
}

// `disabled` must survive a config.toml render→parse round-trip, matching
// `last_resort_round_trips_through_config_toml` above. `off` (the default)
// must render as a comment, not a live key — mirroring every sibling
// default-off boolean's on-disk shape.
#[test]
fn disabled_round_trips_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.disabled = true;
    let rendered = render_config_toml(&profile);
    assert!(
        rendered.contains("disabled = true"),
        "disabled=true must be a real, uncommented key"
    );
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert!(parsed.disabled);

    let off = Profile::new("p".to_string(), None, None);
    let rendered_off = render_config_toml(&off);
    assert!(
        !rendered_off.contains("\ndisabled = true"),
        "disabled=false (the default) must be omitted entirely, not written as a live key"
    );
    let parsed_off: ProfileConfig = toml::from_str(&rendered_off).expect("parse rendered toml");
    assert!(!parsed_off.disabled);
}

// The per-account usage gates must default ON (unset = `None` = checked) so
// every config.toml written before they existed keeps its stock gating.
#[test]
fn profile_config_usage_gates_default_unset() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert_eq!(cfg.check_weekly, None);
    assert_eq!(cfg.check_scoped, None);
}

// Only the non-default (`false`) value renders uncommented, and it must
// survive a render→parse round-trip; the default renders as a commented
// example that parses back to unset.
#[test]
fn usage_gates_round_trip_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.check_weekly = false;
    profile.check_scoped = false;
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.check_weekly, Some(false));
    assert_eq!(parsed.check_scoped, Some(false));

    let stock = render_config_toml(&Profile::new("p".to_string(), None, None));
    let parsed: ProfileConfig = toml::from_str(&stock).expect("parse stock toml");
    assert_eq!(parsed.check_weekly, None);
    assert_eq!(parsed.check_scoped, None);
}

// `burn_aware_switching` (issue #8 follow-up b) must default to `false` so
// every existing profiles.toml written before this field existed keeps
// loading unchanged, matching the `last_resort` guarantee above at the
// `AppState` level.
#[test]
fn app_state_burn_aware_switching_defaults_false() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(!state.burn_aware_switching);
}

#[test]
fn app_state_reads_burn_aware_switching_true() {
    let toml = "profiles = []\nburn_aware_switching = true\n";
    let state: AppState = toml::from_str(toml).expect("parse state");
    assert!(state.burn_aware_switching);
}

// On must round-trip explicitly; off (the default) is omitted entirely from
// the rendered profiles.toml, matching `show_pace`/`count_cache`'s treatment
// of their own default-off booleans.
#[test]
fn burn_aware_switching_round_trips_and_is_omitted_when_off() {
    let on = AppState {
        burn_aware_switching: true,
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains("burn_aware_switching = true"),
        "on must render explicitly, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert!(reparsed.burn_aware_switching);

    let off = AppState::default();
    let rendered_off = toml::to_string_pretty(&off).expect("render default state");
    assert!(
        !rendered_off.contains("burn_aware_switching"),
        "off (default) must be omitted, got:\n{rendered_off}"
    );
}

// `preemptive_rotation` defaults ON, so it takes `refresh_spent_accounts`'s
// default-true serde contract, not `burn_aware_switching`'s: a state file
// written before the key existed must read as ON (`serde(default)` alone would
// hand back `false` whatever `AppState::default()` says), and an explicitly-OFF
// toggle must be WRITTEN — `skip_serializing_if = "is_false"` would drop the
// key and the next load would silently turn it back on.
#[test]
fn preemptive_rotation_defaults_true_and_an_explicit_off_survives_a_round_trip() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(
        state.preemptive_rotation,
        "a state file predating the key must read as the new default (on)"
    );
    assert!(AppState::default().preemptive_rotation);

    let off = AppState {
        preemptive_rotation: false,
        ..AppState::default()
    };
    let rendered_off = toml::to_string_pretty(&off).expect("render off state");
    assert!(
        rendered_off.contains("preemptive_rotation = false"),
        "off must render explicitly or the next load reverts it to on, got:\n{rendered_off}"
    );
    let reparsed: AppState = toml::from_str(&rendered_off).expect("reparse off state");
    assert!(
        !reparsed.preemptive_rotation,
        "the operator's off must survive save + reload"
    );

    let rendered_on = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_on.contains("preemptive_rotation"),
        "on (default) must be omitted, got:\n{rendered_on}"
    );
}

// `auto_rescue` was the opt-in behind the isolated-transcript rescue, which every
// isolated run now gets unconditionally. `AppState` carries no
// `deny_unknown_fields`, so a profiles.toml written while the key existed still
// loads — the key is ignored rather than refused, and nothing renders it back.
// The load half is the one that matters: refusing it would lock an operator out
// of every account on the first launch after an upgrade.
#[test]
fn a_profiles_toml_carrying_the_removed_auto_rescue_key_still_loads() {
    let state: AppState = toml::from_str("profiles = []\nauto_rescue = true\n")
        .expect("a state file from before the key was removed must still parse");
    assert!(
        state.active_profile.is_none() && state.profiles.is_empty(),
        "the rest of the file loads as it always did"
    );

    let rendered = toml::to_string_pretty(&state).expect("render state");
    assert!(
        !rendered.contains("auto_rescue"),
        "the key is dropped on the next save, not carried forward: \n{rendered}"
    );
}

// The reset-display pair (issue #39) renders as its own on-disk vocabulary, so
// these pin the literal keys AND values rather than round-tripping through the
// same enum both ways: a renamed variant would keep a round-trip green while
// every existing profiles.toml silently reverted to the default.
#[test]
fn reset_display_pair_round_trips_and_is_omitted_at_the_default() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert_eq!(state.reset_display(), ResetDisplay::Relative);
    assert_eq!(state.clock_format(), ClockFormat::H24);

    let on = AppState {
        reset_display: Some(ResetDisplay::Both),
        clock_format: Some(ClockFormat::H12),
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains(r#"reset_display = "both""#),
        "reset_display renders its lowercase name, got:\n{rendered_on}"
    );
    assert!(
        rendered_on.contains(r#"clock_format = "12h""#),
        "clock_format renders as 12h/24h, not the Rust variant, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert_eq!(reparsed.reset_display(), ResetDisplay::Both);
    assert_eq!(reparsed.clock_format(), ClockFormat::H12);

    // 24h is the default but still a real stored choice: it must survive a
    // round trip rather than reading back as "never set".
    let h24 = AppState {
        clock_format: Some(ClockFormat::H24),
        ..AppState::default()
    };
    let rendered_h24 = toml::to_string_pretty(&h24).expect("render 24h state");
    assert!(rendered_h24.contains(r#"clock_format = "24h""#));

    let rendered_off = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_off.contains("reset_display") && !rendered_off.contains("clock_format"),
        "an untouched state file gains neither key, got:\n{rendered_off}"
    );
}

// `refresh_spent_accounts` defaults to TRUE (poll every account — today's
// behavior) so pre-field profiles.toml files load unchanged; only an explicit
// `false` opt-out renders, and the default is omitted (the inverse serde shape
// of the default-off toggles above, matching `show_estimates`).
#[test]
fn refresh_spent_accounts_defaults_true_and_round_trips() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(state.refresh_spent_accounts, "absent → default on");

    let off = AppState {
        refresh_spent_accounts: false,
        ..AppState::default()
    };
    let rendered_off = toml::to_string_pretty(&off).expect("render off state");
    assert!(
        rendered_off.contains("refresh_spent_accounts = false"),
        "an explicit opt-out must render, got:\n{rendered_off}"
    );
    let reparsed: AppState = toml::from_str(&rendered_off).expect("reparse off state");
    assert!(!reparsed.refresh_spent_accounts);

    let rendered_on = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_on.contains("refresh_spent_accounts"),
        "default (on) must be omitted, got:\n{rendered_on}"
    );
}

#[test]
fn profile_name_is_serde_transparent() {
    // `ProfileName` must serialize as a bare string so profiles.toml stays
    // byte-identical to the pre-newtype format (a non-transparent newtype
    // would silently migrate every user's state file).
    let toml = r#"active_profile = "work"
profiles = ["work", "play"]
fallback_chain = ["work"]
"#;
    let state: AppState = toml::from_str(toml).expect("parse bare-string state");
    assert_eq!(state.active_profile.as_deref(), Some("work"));
    assert_eq!(state.profiles, ["work", "play"]);
    assert_eq!(state.fallback_chain, ["work"]);

    let rendered = toml::to_string_pretty(&state).expect("render state");
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse");
    assert_eq!(reparsed.active_profile.as_deref(), Some("work"));
    assert_eq!(reparsed.profiles, ["work", "play"]);
    assert_eq!(reparsed.fallback_chain, ["work"]);
    assert!(
        rendered.contains("active_profile = \"work\""),
        "active_profile must render as a bare string, got:\n{rendered}"
    );
    assert!(
        rendered.contains("\"work\"") && rendered.contains("\"play\""),
        "profile names must render as bare strings, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("ProfileName") && !rendered.contains("[profiles."),
        "no newtype wrapper may appear on disk, got:\n{rendered}"
    );

    // Byte-for-byte equality with a String-typed control — no format migration.
    // Field order and serde attrs mirror `AppState`'s ON-DISK shape exactly, so
    // this field is spelled `wrap_off` (the published key) rather than
    // `switch_off_when_spent` (the Rust name behind `serde(rename)`).
    #[derive(serde::Serialize, Default)]
    struct BareState {
        active_profile: Option<String>,
        profiles: Vec<String>,
        fallback_chain: Vec<String>,
        wrap_off: bool,
        refresh_interval_ms: u64,
    }
    let control = BareState {
        active_profile: Some("work".to_string()),
        profiles: vec!["work".to_string(), "play".to_string()],
        fallback_chain: vec!["work".to_string()],
        refresh_interval_ms: 90_000,
        ..Default::default()
    };
    assert_eq!(
        rendered,
        toml::to_string_pretty(&control).expect("render control"),
        "ProfileName AppState must serialize byte-identically to a String one"
    );
}

// ── AUTH-1: `auth_broken` quarantine set semantics + persistence ──────────────

// `set_auth_broken` returns whether the set actually changed — the transition
// signal `mark_auth_broken` keys its single stderr line off of. Both directions
// flip once and then no-op.
#[test]
fn set_auth_broken_reports_transitions_and_is_idempotent() {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    assert!(
        config.set_auth_broken(&crate::profile::ProfileName::from("x"), true),
        "clear→broken is a transition"
    );
    assert!(config.is_auth_broken(&crate::profile::ProfileName::from("x")));
    assert!(
        !config.set_auth_broken(&crate::profile::ProfileName::from("x"), true),
        "broken→broken is a no-op (no duplicate log)"
    );
    assert!(
        config.set_auth_broken(&crate::profile::ProfileName::from("x"), false),
        "broken→clear is a transition"
    );
    assert!(!config.is_auth_broken(&crate::profile::ProfileName::from("x")));
    assert!(
        !config.set_auth_broken(&crate::profile::ProfileName::from("x"), false),
        "clear→clear is a no-op"
    );
}

// A quarantined account must survive a save/load of profiles.toml, and an older
// state file written before the field existed must still load (serde default →
// empty), or upgrading would either forget a dead login or fail to parse.
#[test]
fn auth_broken_round_trips_and_is_omitted_when_empty() {
    let on = AppState {
        auth_broken: vec!["dead".into()],
        ..AppState::default()
    };
    let rendered = toml::to_string_pretty(&on).expect("render quarantined state");
    assert!(
        rendered.contains("auth_broken"),
        "a populated quarantine must render, got:\n{rendered}"
    );
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse quarantined state");
    assert_eq!(
        reparsed
            .auth_broken
            .iter()
            .map(ProfileName::as_str)
            .collect::<Vec<_>>(),
        ["dead"],
        "the quarantined name survives the round-trip"
    );

    let rendered_off = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_off.contains("auth_broken"),
        "an empty quarantine is omitted from disk, got:\n{rendered_off}"
    );

    let older: AppState = toml::from_str("profiles = []\n").expect("parse pre-field state");
    assert!(
        older.auth_broken.is_empty(),
        "a state file without the field defaults to an empty quarantine"
    );
}

// `remove` must drop the removed name from the quarantine list too — a stale
// entry would otherwise linger and could re-attach to a re-created same-name
// profile.
#[test]
fn remove_drops_auth_broken_entry() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["a".into(), "b".into()],
            ..AppState::default()
        },
        profiles: vec![
            Profile::new("a".to_string(), None, None),
            Profile::new("b".to_string(), None, None),
        ],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("a"), true);
    config.set_auth_broken(&crate::profile::ProfileName::from("b"), true);
    crate::lock::with_state_lock(|held| {
        config.remove(&crate::profile::ProfileName::from("a"), held);
        Ok(())
    })
    .expect("remove");
    assert!(
        !config.is_auth_broken(&crate::profile::ProfileName::from("a")),
        "removed name leaves the quarantine"
    );
    assert!(
        config.is_auth_broken(&crate::profile::ProfileName::from("b")),
        "the other quarantine is untouched"
    );
}

// `rename_all_occurrences` must carry the quarantine to the new name — a rename
// that dropped it would silently un-quarantine a dead login.
#[test]
fn rename_carries_auth_broken_entry() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["old".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("old".to_string(), None, None)],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("old"), true);
    crate::lock::with_state_lock(|held| {
        config.rename_all_occurrences(
            &crate::profile::ProfileName::from("old"),
            &crate::profile::ProfileName::from("new"),
            held,
        );
        Ok(())
    })
    .expect("rename");
    assert!(
        !config.is_auth_broken(&crate::profile::ProfileName::from("old")),
        "old name no longer quarantined"
    );
    assert!(
        config.is_auth_broken(&crate::profile::ProfileName::from("new")),
        "quarantine follows the rename"
    );
}

// Ungated on purpose, unlike the mode assertions around them:
// `disabling_persists_and_leaves_credentials_byte_unchanged` asserts bytes and
// dir entries rather than modes, so it runs on windows too.
use crate::testutil::HomeSandbox;

fn oauth_credentials() -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "tok-access".to_string(),
            refresh_token: Some("tok-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// Out-of-band per-profile thresholds are CLAMPED to the band at load, while the
/// app-level weekly line RESETS TO DEFAULT (pinned separately by
/// `weekly_switch_threshold_out_of_band_resets_to_default_at_load`). Two
/// deliberately different normalizations, one line apart in the source —
/// exactly the shape a well-meaning "unify
/// the threshold handling" refactor collapses into one rule, silently moving
/// every hand-edited config to the wrong value. A garbage `fallback_threshold`
/// left raw would also drive the auto-switch walk off a nonsense line, so the
/// clamp is load-bearing rather than cosmetic. Both fields, both directions.
#[test]
fn out_of_band_per_profile_thresholds_clamp_to_the_band_at_load() {
    let _home = HomeSandbox::new();
    let name = "clamp-test";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");

    // Hand-edit the per-profile config the way a user would.
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");
    std::fs::write(
        &config_path,
        "fallback_threshold = 250.0\nbell_threshold = -30.0\n",
    )
    .expect("write config.toml");

    let loaded = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(
        loaded.fallback_threshold,
        Some(100.0),
        "an over-band fallback_threshold clamps to the top of the band, it does not \
         reset to default and is never left raw",
    );
    assert_eq!(
        loaded.bell_threshold,
        Some(0.0),
        "an under-band bell_threshold clamps to the bottom of the band",
    );

    // In-band values are untouched — the clamp must not round or default them.
    std::fs::write(
        &config_path,
        "fallback_threshold = 73.5\nbell_threshold = 12.0\n",
    )
    .expect("rewrite config.toml");
    let loaded = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(loaded.fallback_threshold, Some(73.5));
    assert_eq!(loaded.bell_threshold, Some(12.0));
}

/// The Rust field is `switch_off_when_spent`; the ON-DISK key must stay
/// `wrap_off`. Nothing else pins this: `status.json`'s contract test covers its
/// own key, and every round-trip test goes through serde in both directions, so
/// a rename of the serde name passes them all while silently resetting the
/// setting to `false` in every profiles.toml already on disk. A blind
/// find-and-replace across the field name did exactly that (2026-07-17), which
/// is what this test exists to catch.
#[test]
fn switch_off_when_spent_keeps_its_wrap_off_key_on_disk() {
    let from_disk: AppState = toml::from_str("profiles = []\nwrap_off = true\n")
        .expect("the legacy key must still parse");
    assert!(
        from_disk.switch_off_when_spent,
        "an existing profiles.toml's `wrap_off = true` must survive the rename"
    );

    let rendered = toml::to_string(&AppState {
        switch_off_when_spent: true,
        ..AppState::default()
    })
    .expect("serialize");
    assert!(
        rendered.contains("wrap_off = true"),
        "writes must keep the published key, else an older clauth reads the file \
         and silently loses the setting: {rendered}"
    );
    assert!(
        !rendered.contains("switch_off_when_spent"),
        "the Rust name must not reach disk: {rendered}"
    );
}

/// `max_auto_spend` is a dollar ceiling on unattended spending, so its load
/// normalization is a money guard, not a tidy-up. `inf` and `nan` are both
/// valid TOML floats: left raw, an infinite ceiling means an account with no
/// declared cap has infinite room (`fallback::spend_room`), i.e. unbounded
/// spending from one hand-edited word. Anything non-finite reads as the
/// never-spend default instead.
#[test]
fn non_finite_max_auto_spend_reads_as_zero_at_load() {
    let _home = HomeSandbox::new();
    let name = "spend-ceiling-test";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");

    for raw in ["max_auto_spend = inf\n", "max_auto_spend = nan\n"] {
        std::fs::write(&config_path, raw).expect("write config.toml");
        assert_eq!(
            load_profile(&crate::profile::ProfileName::from(name))
                .expect("load_profile")
                .max_auto_spend,
            Some(0.0),
            "{raw:?} must not survive the load boundary as a spendable ceiling"
        );
    }

    // A negative ceiling floors at $0 rather than staying raw...
    std::fs::write(&config_path, "max_auto_spend = -5.0\n").expect("write config.toml");
    assert_eq!(
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .max_auto_spend,
        Some(0.0)
    );

    // ...and an ordinary ceiling is passed through untouched.
    std::fs::write(&config_path, "max_auto_spend = 12.5\n").expect("write config.toml");
    assert_eq!(
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .max_auto_spend,
        Some(12.5)
    );
}

/// `nan` is a valid TOML float that survives `clamp`, and every `>=` against a
/// NaN threshold reads false, so a hand-edited one silently disables the gate
/// it was meant to set. Worse, `render_config_toml` writes it back out as
/// `NaN`, which TOML rejects, so the next `load_profile` fails on the file
/// clauth itself just rewrote. Non-finite reads as unset on both percent
/// fields, matching `max_auto_spend`'s guard above.
#[test]
fn non_finite_percent_fields_read_as_unset_at_load() {
    let _home = HomeSandbox::new();
    let name = "finite-pct-test";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");

    for raw in ["nan", "inf", "-inf"] {
        std::fs::write(&config_path, format!("fallback_threshold = {raw}\n"))
            .expect("write config.toml");
        assert_eq!(
            load_profile(&crate::profile::ProfileName::from(name))
                .expect("load_profile")
                .fallback_threshold,
            None,
            "fallback_threshold = {raw} must not survive the load boundary"
        );
        // The rewrite that load just performed has to still be parseable.
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("re-load after the fallback_threshold rewrite");

        std::fs::write(&config_path, format!("bell_threshold = {raw}\n"))
            .expect("write config.toml");
        assert_eq!(
            load_profile(&crate::profile::ProfileName::from(name))
                .expect("load_profile")
                .bell_threshold,
            None,
            "bell_threshold = {raw} must not survive the load boundary"
        );
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("re-load after the bell_threshold rewrite");
    }
}

/// The load boundary drops a base_url ONLY when a stored OAuth pair could leak
/// to it (pair present + no usable key). A pure api account with a cleared key
/// keeps its base_url shell so `clear_profile_api_key` stays re-loginable — the
/// same normalize-at-load discipline as `max_auto_spend`, scoped to the leak.
#[test]
fn base_url_dropped_only_when_a_stored_pair_could_leak() {
    let _home = HomeSandbox::new();
    let name = "endpoint-key-gate";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let endpoint = "https://api.z.ai/anthropic";

    // No stored pair + no key: nothing to leak, so the base_url shell is kept —
    // a cleared api account (`clear_profile_api_key`) must stay re-loginable.
    std::fs::write(&config_path, format!("base_url = \"{endpoint}\"\n")).expect("write config");
    let pure = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(
        pure.base_url.as_deref(),
        Some(endpoint),
        "no pair means no leak, so the base_url shell is kept"
    );

    // Seed an OAuth pair (a hybrid). With no key the pair would reach the
    // endpoint, so base_url (and its provider) is dropped; CC routes to Anthropic.
    std::fs::write(
        &cred_path,
        serde_json::to_string(&oauth_credentials()).expect("ser creds"),
    )
    .expect("write credentials.json");
    let hybrid = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(
        hybrid.base_url, None,
        "a stored pair with no key must not route to the endpoint"
    );
    assert!(
        hybrid.provider.is_none(),
        "a dropped endpoint has no provider"
    );

    // A whitespace-only key is still no usable key → still dropped.
    std::fs::write(
        &config_path,
        format!("base_url = \"{endpoint}\"\napi_key = \"   \"\n"),
    )
    .expect("write config");
    assert_eq!(
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .base_url,
        None
    );

    // A real key on the same hybrid keeps the endpoint and its provider.
    std::fs::write(
        &config_path,
        format!("base_url = \"{endpoint}\"\napi_key = \"sk-real\"\n"),
    )
    .expect("write config");
    let keyed = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(keyed.base_url.as_deref(), Some(endpoint));
    assert!(
        keyed.provider.is_some(),
        "a keyed z.ai endpoint keeps its provider"
    );
}

/// `stored_usage_cache_is_third_party` answers the question
/// `load_profile(&crate::profile::ProfileName::from(…)).usage_cache_is_third_party()` answers, without recovering a
/// staged rotation — that recovery takes the state flock and rewrites
/// `credentials.json`, which a caller under a leaf lock (the MCP digest's 5 Hz
/// sample) can never do.
///
/// Two readers of one rule, so they are pinned to agree across every state the
/// rule branches on: both disjuncts (a recognised provider, and a generic
/// endpoint with a key) and both answers, or a reader stuck on either constant
/// passes. The ONE state where they legitimately disagree is pinned separately
/// below, in its direction, rather than left out of a docstring claiming
/// agreement everywhere.
#[test]
fn the_lock_free_third_party_read_agrees_with_a_full_load() {
    let _home = HomeSandbox::new();
    let name = "endpoint-agreement";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let known = "https://api.z.ai/anthropic";
    // No typed integration claims this one, so it exercises the second disjunct
    // (`base_url` + `api_key`) that `provider.is_some()` alone never reaches.
    let generic = "http://127.0.0.1:4000";
    let creds = serde_json::to_string(&oauth_credentials()).expect("ser creds");

    let mut seen = Vec::new();
    for (label, config, pair) in [
        ("no endpoint at all", String::new(), false),
        (
            "recognised endpoint, no pair, no key",
            format!("base_url = \"{known}\"\n"),
            false,
        ),
        (
            "recognised endpoint + pair, no usable key",
            format!("base_url = \"{known}\"\napi_key = \"   \"\n"),
            true,
        ),
        (
            "recognised endpoint + pair + real key",
            format!("base_url = \"{known}\"\napi_key = \"sk-real\"\n"),
            true,
        ),
        (
            "generic endpoint + key",
            format!("base_url = \"{generic}\"\napi_key = \"sk-real\"\n"),
            false,
        ),
        (
            "generic endpoint, no key",
            format!("base_url = \"{generic}\"\n"),
            false,
        ),
    ] {
        std::fs::write(&config_path, &config).expect("write config");
        if pair {
            std::fs::write(&cred_path, &creds).expect("write credentials.json");
        } else {
            let _ = std::fs::remove_file(&cred_path);
        }
        let full = load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .usage_cache_is_third_party();
        assert_eq!(
            stored_usage_cache_is_third_party(&crate::profile::ProfileName::from(name)),
            full,
            "the lock-free read disagrees with the load boundary on: {label}",
        );
        seen.push(full);
    }
    assert!(
        seen.contains(&true) && seen.contains(&false),
        "the fixture must exercise both answers, or a constant reader passes: {seen:?}",
    );
    assert!(
        seen[4],
        "a generic api-key endpoint's usage lives in the third-party cache too, \
         which is the half `provider.is_some()` answers wrong: {seen:?}",
    );
}

/// The one state the two readers do NOT agree on, pinned in its direction so it
/// stays a known cost rather than a surprise: a pair staged as
/// `credentials.json.pending` and never committed. The lock-free read stats the
/// COMMITTED file only, so it sees no credentials, keeps the `base_url` that a
/// full load would drop (the pair would otherwise reach the endpoint), and
/// answers `true` where `load_profile` answers `false`.
///
/// Only the MCP digest's sample can observe it — every other caller runs
/// `load_config` first, and `recover_pending_credentials` consumes the sidecar —
/// and the cost is one digest call watching the wrong cache, so no refresh is
/// reported for it.
#[test]
fn a_staged_pair_is_the_one_state_the_lock_free_read_reads_differently() {
    let _home = HomeSandbox::new();
    let name = "endpoint-staged";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");
    let endpoint = "https://api.z.ai/anthropic";
    std::fs::write(&config_path, format!("base_url = \"{endpoint}\"\n")).expect("write config");
    // Staged but never committed: no `credentials.json` on disk.
    let pending = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    std::fs::write(
        &pending,
        serde_json::to_string(&oauth_credentials()).expect("ser creds"),
    )
    .expect("write pending sidecar");

    // The lock-free read FIRST: `load_profile` consumes the sidecar, and after
    // that there is nothing left to disagree about.
    assert!(
        stored_usage_cache_is_third_party(&crate::profile::ProfileName::from(name)),
        "the committed file is empty, so this read keeps the endpoint",
    );
    assert!(
        !load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .usage_cache_is_third_party(),
        "the full load adopts the staged pair and drops the endpoint with it",
    );
}

// ── crash-durable rotation: the pending sidecar's adopt/discard decision ─────
//
// `stage_rotated_credentials` writes a rotated pair to `credentials.json.pending`
// BEFORE `save_profile`, so a crash between the OAuth response and the commit
// can't lose a single-use refresh token. That guarantee reduces to ONE mtime
// compare in
// `recover_pending_credentials`, and until now only the sidecar's file *mode* was
// tested — never the decision. Both ways of getting it wrong are silent and
// unrecoverable: adopt too eagerly and a clean commit is overwritten by the pair
// it already superseded (a spent token reinstalled, next refresh 400s), discard
// too eagerly and a genuinely orphaned rotation is dropped (that pair is gone
// and the account needs a manual re-login). Each arm below is one of those.

fn pair(access: &str, refresh: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn refresh_token_of(creds: &Option<ClaudeCredentials>) -> Option<&str> {
    creds
        .as_ref()?
        .claude_ai_oauth
        .as_ref()?
        .refresh_token
        .as_deref()
}

fn seed_committed(name: &str, creds: &ClaudeCredentials) {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    profile.credentials = Some(creds.clone());
    save_profile(&profile).expect("save_profile");
}

/// Sidecar NEWER than `credentials.json`: the rotation was staged but the commit
/// never landed, so the staged pair is the only live one — adopt it, write it
/// through to `credentials.json`, and consume the sidecar.
#[test]
fn pending_sidecar_newer_than_the_commit_is_adopted_and_written_through() {
    let _home = HomeSandbox::new();
    let name = "pending-adopt-newer";
    let committed = pair("old-access", "old-refresh");
    seed_committed(name, &committed);

    let staged = pair("new-access", "new-refresh");
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &staged)
        .expect("stage_rotated_credentials");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&pending_path, now);

    let got = recover_pending_credentials(
        &crate::profile::ProfileName::from(name),
        Some(committed.clone()),
    );
    assert_eq!(
        refresh_token_of(&got),
        Some("new-refresh"),
        "a rotation staged after the last commit is the live pair and must be adopted",
    );

    // Written through, so the next load sees it even without the sidecar.
    let on_disk: ClaudeCredentials = read_json_file(&cred_path).expect("re-read credentials.json");
    assert_eq!(
        on_disk
            .claude_ai_oauth
            .and_then(|o| o.refresh_token)
            .as_deref(),
        Some("new-refresh"),
        "the adopted pair must be committed to credentials.json, not just returned",
    );
    assert!(
        !pending_path.exists(),
        "the sidecar must be consumed so the next load can't adopt it a second time",
    );
}

/// Sidecar OLDER than `credentials.json`: the commit landed cleanly and the
/// sidecar is its already-superseded predecessor. Adopting it would reinstall a
/// spent refresh token, so it must be discarded — and still cleaned up.
#[test]
fn pending_sidecar_older_than_the_commit_is_discarded_not_reinstalled() {
    let _home = HomeSandbox::new();
    let name = "pending-discard-older";
    let committed = pair("live-access", "live-refresh");
    seed_committed(name, &committed);

    let superseded = pair("spent-access", "spent-refresh");
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &superseded)
        .expect("stage_rotated_credentials");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&pending_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&cred_path, now);

    let got = recover_pending_credentials(
        &crate::profile::ProfileName::from(name),
        Some(committed.clone()),
    );
    assert_eq!(
        refresh_token_of(&got),
        Some("live-refresh"),
        "a commit newer than the sidecar already won; reinstalling the sidecar would \
         resurrect a spent refresh token",
    );

    let on_disk: ClaudeCredentials = read_json_file(&cred_path).expect("re-read credentials.json");
    assert_eq!(
        on_disk
            .claude_ai_oauth
            .and_then(|o| o.refresh_token)
            .as_deref(),
        Some("live-refresh"),
        "a discarded sidecar must not touch credentials.json",
    );
    assert!(
        !pending_path.exists(),
        "even a discarded sidecar is cleaned up, or it is re-evaluated on every load",
    );
}

/// The boundary is `>=`, not `>`: equal mtimes adopt. Staging and committing
/// within one filesystem timestamp tick is the common case on a coarse-grained
/// mtime, and treating that as "the commit won" would drop a rotation that may
/// never have landed.
#[test]
fn pending_sidecar_with_an_equal_mtime_is_adopted() {
    let _home = HomeSandbox::new();
    let name = "pending-adopt-equal";
    let committed = pair("old-access", "old-refresh");
    seed_committed(name, &committed);

    let staged = pair("tie-access", "tie-refresh");
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &staged)
        .expect("stage_rotated_credentials");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let same = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred_path, same);
    crate::testutil::set_mtime(&pending_path, same);

    assert_eq!(
        refresh_token_of(&recover_pending_credentials(
            &crate::profile::ProfileName::from(name),
            Some(committed)
        )),
        Some("tie-refresh"),
        "an equal mtime must adopt: the compare is `pending >= committed`",
    );
}

/// No `credentials.json` at all (the crash landed between staging and the first
/// commit): there is nothing to compare against and the sidecar is the only pair
/// in existence — adopt unconditionally rather than treating the missing file as
/// a reason to discard.
#[test]
fn pending_sidecar_is_adopted_when_no_commit_exists_at_all() {
    let _home = HomeSandbox::new();
    let name = "pending-adopt-absent";
    // Seed the profile dir without credentials so only the sidecar exists.
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");

    let staged = pair("only-access", "only-refresh");
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &staged)
        .expect("stage_rotated_credentials");
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    assert!(
        !cred_path.exists(),
        "precondition: no committed credentials"
    );

    assert_eq!(
        refresh_token_of(&recover_pending_credentials(
            &crate::profile::ProfileName::from(name),
            None
        )),
        Some("only-refresh"),
        "with no commit to compare against, the staged pair is the only live one",
    );
}

/// The adopt rule compares WRITE times, and a per-session swap moves a store's
/// mtime with no bytes behind it so Claude Code re-reads it. Reading that stamp
/// as a commit discards a sidecar staged before it — a refresh pair that may be
/// the only live one, gone on the next load with nothing left to recover it.
#[test]
fn a_bare_store_stamp_does_not_discard_a_sidecar_staged_before_it() {
    let _home = HomeSandbox::new();
    let name = "pending-stamped-store";
    // The receipt below is a cache write, gated on the on-disk record.
    crate::testutil::register_names(&[name]);
    let committed = pair("old-access", "old-refresh");
    seed_committed(name, &committed);

    let staged = pair("orphan-access", "orphan-refresh");
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &staged)
        .expect("stage_rotated_credentials");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let now = std::time::SystemTime::now();
    let last_write = now - std::time::Duration::from_secs(120);
    crate::testutil::set_mtime(&cred_path, last_write);
    crate::testutil::set_mtime(&pending_path, now - std::time::Duration::from_secs(60));

    // What a swap onto this member leaves: the store's mtime moved to `now`, no
    // byte of it written, and the receipt that says so.
    crate::profile_cache::write_touch_receipt(
        &crate::profile::ProfileName::from(name),
        &cred_path,
        now,
        Some(last_write),
    );
    crate::testutil::set_mtime(&cred_path, now);

    assert_eq!(
        refresh_token_of(&recover_pending_credentials(
            &crate::profile::ProfileName::from(name),
            Some(committed)
        )),
        Some("orphan-refresh"),
        "the stamp moved no bytes, so the staged pair is still the newest write",
    );
}

/// The other direction of the same rule: a real commit landing after the stamp
/// moves the store's mtime off the receipt, which retires it. The sidecar is a
/// superseded predecessor again and reinstalling it would resurrect a spent
/// refresh token.
#[test]
fn a_commit_landing_after_a_stamp_still_discards_the_sidecar() {
    let _home = HomeSandbox::new();
    let name = "pending-stamped-then-committed";
    let superseded = pair("spent-access", "spent-refresh");
    seed_committed(name, &superseded);
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &superseded)
        .expect("stage_rotated_credentials");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let now = std::time::SystemTime::now();
    let stamped = now - std::time::Duration::from_secs(30);
    crate::testutil::set_mtime(&pending_path, now - std::time::Duration::from_secs(60));
    crate::profile_cache::write_touch_receipt(
        &crate::profile::ProfileName::from(name),
        &cred_path,
        stamped,
        Some(now - std::time::Duration::from_secs(120)),
    );
    crate::testutil::set_mtime(&cred_path, stamped);

    // A rotation commits after the swap: real bytes, and an mtime the receipt
    // no longer describes.
    let live = pair("live-access", "live-refresh");
    seed_committed(name, &live);
    crate::testutil::set_mtime(&cred_path, now);

    assert_eq!(
        refresh_token_of(&recover_pending_credentials(
            &crate::profile::ProfileName::from(name),
            Some(live)
        )),
        Some("live-refresh"),
        "a commit newer than the sidecar still wins; the stamp's receipt is spent",
    );
}

/// A receipt names the store it stamped. A profile can hold both a
/// `credentials.json` and a `session-token.json`, a swap stamps exactly one of
/// them, and a coarse-granularity mtime ticks the two together — so resolving one
/// store through the other's receipt would report a write time that never was.
#[test]
fn a_touch_receipt_only_resolves_the_store_it_names() {
    let _home = HomeSandbox::new();
    let name = "receipt-scope";
    crate::testutil::register_names(&[name]);
    seed_committed(name, &pair("access", "refresh"));
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let sidecar = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "session-token.json",
    )
    .expect("sidecar path");
    std::fs::write(&sidecar, b"{}\n").expect("write sidecar");

    let now = std::time::SystemTime::now();
    let displaced = now - std::time::Duration::from_secs(300);
    crate::testutil::set_mtime(&cred_path, now);
    crate::testutil::set_mtime(&sidecar, now);
    crate::profile_cache::write_touch_receipt(
        &crate::profile::ProfileName::from(name),
        &sidecar,
        now,
        Some(displaced),
    );

    assert_eq!(
        crate::profile_cache::effective_write_time(&sidecar),
        Some(displaced),
        "the stamped store resolves to the write its stamp displaced",
    );
    assert_eq!(
        crate::profile_cache::effective_write_time(&cred_path),
        Some(now),
        "a store the receipt does not name keeps its own mtime, tie or not",
    );
}

/// `scopes_joined` feeds the refresh `scope` field (Claude Code echoes its
/// credential's granted scopes on refresh). Order must survive and an empty set
/// must read as `None` so the refresh path falls back instead of sending `""`.
#[test]
fn scopes_joined_space_joins_preserving_order_and_maps_empty_to_none() {
    let creds = |scopes: Option<Vec<String>>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            expires_at: None,
            scopes,
            subscription_type: None,
        }),
    };
    assert_eq!(
        creds(Some(vec!["user:profile".into(), "user:inference".into()])).scopes_joined(),
        Some("user:profile user:inference".to_string())
    );
    assert_eq!(creds(Some(Vec::new())).scopes_joined(), None);
    assert_eq!(creds(None).scopes_joined(), None);
    assert_eq!(
        ClaudeCredentials {
            claude_ai_oauth: None
        }
        .scopes_joined(),
        None
    );
}

/// credentials.json, its `.pending` rotation sidecar, and the per-profile dir
/// must carry tightened permissions: 0o600 files, 0o700 dir.
#[cfg(unix)]
#[test]
fn credential_and_cache_files_have_restricted_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let name = "perm-test-credentials";
    let creds = oauth_credentials();

    let profile = Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: std::collections::BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: Some(creds.clone()),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    // Goes through ConfigHandle-equivalent path: save_profile takes the state
    // flock (rank-ordered) and writes credentials.json before config.toml.
    save_profile(&profile).expect("save_profile");

    let dir_mode = std::fs::metadata(
        profile_dir(&crate::profile::ProfileName::from(name)).expect("profile_dir"),
    )
    .expect("dir metadata")
    .permissions()
    .mode();
    assert_eq!(
        dir_mode & 0o777,
        0o700,
        "profile dir mode should be 0o700, got {:#o}",
        dir_mode & 0o777,
    );

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let cred_mode = std::fs::metadata(&cred_path)
        .expect("credentials.json metadata")
        .permissions()
        .mode();
    assert_eq!(
        cred_mode & 0o777,
        0o600,
        "credentials.json mode should be 0o600, got {:#o}",
        cred_mode & 0o777,
    );

    // Stage the rotation sidecar and assert its mode too.
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &creds)
        .expect("stage_rotated_credentials");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let pending_mode = std::fs::metadata(&pending_path)
        .expect("credentials.json.pending metadata")
        .permissions()
        .mode();
    assert_eq!(
        pending_mode & 0o777,
        0o600,
        "credentials.json.pending mode should be 0o600, got {:#o}",
        pending_mode & 0o777,
    );

    // profiles.toml goes through the same `atomic_write_600` and names every
    // account plus the active one; it was the one state file this test never
    // covered, so a writer swapped back to a plain `fs::write` would land it at
    // the process umask (world-readable on a default 022) with nothing failing.
    save_app_state(&AppState::default()).expect("save_app_state");
    let state_mode = std::fs::metadata(app_state_path().expect("app_state_path"))
        .expect("profiles.toml metadata")
        .permissions()
        .mode();
    assert_eq!(
        state_mode & 0o777,
        0o600,
        "profiles.toml mode should be 0o600, got {:#o}",
        state_mode & 0o777,
    );

    // The swap executor's touch receipt: it holds no secret, but it is a writer
    // under `~/.clauth` and the invariant is the whole tree, so a future writer
    // swapped off the per-profile cache path has to fail here.
    // Registered AFTER the empty `save_app_state` above, which rewrote the
    // record the cache-write gate reads.
    crate::testutil::register_names(&[name]);
    crate::profile_cache::write_touch_receipt(
        &crate::profile::ProfileName::from(name),
        &cred_path,
        std::time::SystemTime::now(),
        None,
    );
    let receipt_mode = std::fs::metadata(
        profile_subpath(
            &crate::profile::ProfileName::from(name),
            crate::profile_cache::TOUCH_RECEIPT_FILE,
        )
        .expect("receipt path"),
    )
    .expect("touch-receipt.json metadata")
    .permissions()
    .mode();
    assert_eq!(
        receipt_mode & 0o777,
        0o600,
        "touch-receipt.json mode should be 0o600, got {:#o}",
        receipt_mode & 0o777,
    );
}

/// Disabling an account is a `config.toml`-only edit: flipping it must persist
/// on reload and never touch the profile directory or stored credentials.
/// `disabled = false` (the default) leaves stock behaviour bit-identical, so
/// this pins both halves of the exclusion feature's storage contract.
#[test]
fn disabling_persists_and_leaves_credentials_byte_unchanged() {
    let _home = HomeSandbox::new();
    let name = "disable-round-trip";
    let mut profile = Profile::new(name.to_string(), None, None);
    profile.credentials = Some(oauth_credentials());
    save_profile(&profile).expect("save_profile (enabled)");
    assert!(
        !load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .is_disabled()
    );

    let cred_path =
        profile_credentials_path(&crate::profile::ProfileName::from(name)).expect("cred path");
    let creds_before = std::fs::read(&cred_path).expect("read credentials.json");
    let mut dir_entries_before: Vec<_> = std::fs::read_dir(
        profile_dir(&crate::profile::ProfileName::from(name)).expect("profile_dir"),
    )
    .expect("read_dir")
    .map(|e| e.expect("dir entry").file_name())
    .collect();
    dir_entries_before.sort_unstable();

    profile.disabled = true;
    save_profile(&profile).expect("save_profile (disabled)");

    let creds_after = std::fs::read(&cred_path).expect("re-read credentials.json");
    assert_eq!(
        creds_before, creds_after,
        "disabling an account must never touch its stored credentials"
    );
    let mut dir_entries_after: Vec<_> = std::fs::read_dir(
        profile_dir(&crate::profile::ProfileName::from(name)).expect("profile_dir"),
    )
    .expect("read_dir")
    .map(|e| e.expect("dir entry").file_name())
    .collect();
    dir_entries_after.sort_unstable();
    assert_eq!(
        dir_entries_before, dir_entries_after,
        "disabling an account must never add or remove files in its profile directory"
    );

    let raw = std::fs::read_to_string(
        profile_config_path(&crate::profile::ProfileName::from(name)).expect("config path"),
    )
    .expect("read config.toml");
    assert!(
        raw.contains("disabled = true"),
        "disabled=true must be a real, serialized key in config.toml"
    );

    let reloaded = load_profile(&crate::profile::ProfileName::from(name))
        .expect("load_profile (&crate::profile::ProfileName::from(disabled))");
    assert!(reloaded.is_disabled(), "reload must observe the toggle");
    assert_eq!(
        reloaded.access_token(),
        profile.access_token(),
        "reload must carry the same credentials through unchanged"
    );
    assert_eq!(reloaded.refresh_token(), profile.refresh_token());
}

/// The real usage-cache writer (`profile_cache::write_profile_cache`) must
/// create usage_cache.json at 0o600 and, when it has to create the per-profile
/// dir, that dir at 0o700. Driven on a FRESH profile name so the dir does not
/// pre-exist.
#[cfg(unix)]
#[test]
fn usage_cache_write_creates_restricted_file_and_dir() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let name = "perm-test-usage-cache";
    // The record carries the name, the dir does not — the write under test is
    // the thing that has to create it.
    crate::testutil::register_names(&[name]);

    // Fresh profile: its dir must not exist before the cache write.
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("profile_dir");
    assert!(
        !dir.exists(),
        "precondition: profile dir must not pre-exist for a fresh profile"
    );

    // Drive the actual production writer.
    let info = crate::usage::UsageInfo::default();
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::USAGE_CACHE_FILE,
        &info,
    );

    let dir_mode = std::fs::metadata(&dir)
        .expect("freshly-created profile dir metadata")
        .permissions()
        .mode();
    assert_eq!(
        dir_mode & 0o777,
        0o700,
        "freshly-created profile dir mode should be 0o700, got {:#o}",
        dir_mode & 0o777,
    );

    let cache_path = profile_subpath(&crate::profile::ProfileName::from(name), "usage_cache.json")
        .expect("cache path");
    let cache_mode = std::fs::metadata(&cache_path)
        .expect("usage_cache.json metadata")
        .permissions()
        .mode();
    assert_eq!(
        cache_mode & 0o777,
        0o600,
        "usage_cache.json mode should be 0o600, got {:#o}",
        cache_mode & 0o777,
    );
}

/// The perms sweep stops at a codex home's threshold: the home NODE keeps the
/// 0700 invariant, while the PATH-alias helper binaries codex plants inside
/// keep their exec bits — a blanket 0600 would break them. The exemption is
/// positional, so a claude profile literally NAMED `codex-home` (the charset
/// allows it) is still a profile dir and still fully retightened.
#[cfg(unix)]
#[test]
fn the_perms_sweep_stops_at_a_codex_homes_threshold() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let clauth = clauth_dir().expect("clauth_dir");

    let codex_home = clauth.join("profiles").join("cx").join("codex-home-4242-0");
    std::fs::create_dir_all(&codex_home).expect("mkdir codex home");
    let helper = codex_home.join("codex-alias");
    std::fs::write(&helper, b"#!/bin/sh\n").expect("write helper");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::set_permissions(&codex_home, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let impostor = clauth.join("profiles").join("codex-home");
    std::fs::create_dir_all(&impostor).expect("mkdir impostor profile");
    std::fs::write(impostor.join("config.toml"), b"").expect("write config");
    std::fs::set_permissions(
        impostor.join("config.toml"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");

    enforce_clauth_perms(&clauth);

    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(
        mode(&codex_home),
        0o700,
        "the home node itself keeps the invariant"
    );
    assert_eq!(
        mode(&helper),
        0o755,
        "the helper binary inside keeps its exec bits"
    );
    assert_eq!(
        mode(&impostor.join("config.toml")),
        0o600,
        "a profile NAMED codex-home is a profile dir, retightened in full"
    );
}

/// Installs from before the 0o600/0o700 rule carry a umask-moded tree that no
/// writer ever revisits: bytes that never change keep their mode forever. Every
/// entry point loads the config, so that is where the tree gets retightened.
#[cfg(unix)]
#[test]
fn load_config_repairs_a_loose_clauth_tree() {
    use crate::testutil::owner_only_violations;
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let name = "perm-test-repair";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    save_app_state(&AppState {
        profiles: vec![name.into()],
        ..Default::default()
    })
    .expect("save_app_state");

    let clauth = clauth_dir().expect("clauth_dir");
    let profile = profile_dir(&crate::profile::ProfileName::from(name)).expect("profile_dir");
    let runtime = profile.join("runtime");
    let sessions = profile.join("sessions");
    std::fs::create_dir_all(&runtime).expect("mkdir runtime");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    std::fs::write(runtime.join("settings.json"), b"{}").expect("write settings");
    std::fs::write(profile.join("usage_history.jsonl"), b"").expect("write history");

    // What an older build left behind: umask modes top to bottom.
    for dir in [&clauth, &clauth.join("profiles"), &profile, &runtime] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod dir");
    }
    for file in [
        profile.join("config.toml"),
        profile.join("usage_history.jsonl"),
        runtime.join("settings.json"),
    ] {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
            .expect("chmod file");
    }

    // A runtime links into the operator's ~/.claude, and `set_permissions`
    // resolves links — walking one would chmod a file clauth does not own.
    let outside = home.home().join("outside.json");
    std::fs::write(&outside, b"{}").expect("write outside");
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::os::unix::fs::symlink(&outside, runtime.join("CLAUDE.md")).expect("symlink");

    load_config().expect("load_config");

    let left = owner_only_violations(&clauth);
    assert!(
        left.is_empty(),
        "load_config must leave the whole ~/.clauth tree owner-only; still loose: {left:#?}"
    );
    let outside_mode = std::fs::metadata(&outside)
        .expect("outside metadata")
        .permissions()
        .mode();
    assert_eq!(
        outside_mode & 0o777,
        0o644,
        "the repair followed a symlink out of the tree and chmodded {:#o} onto a file clauth does not own",
        outside_mode & 0o777,
    );
}

#[test]
fn profile_config_reads_models_table() {
    let toml = "[models]\n\
        default = \"opusplan\"\n\
        haiku = \"claude-haiku-4-5\"\n\
        fable = \"claude-fable-5\"\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse models table");
    assert_eq!(cfg.models.default.as_deref(), Some("opusplan"));
    assert_eq!(cfg.models.haiku.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(cfg.models.fable.as_deref(), Some("claude-fable-5"));
    assert_eq!(cfg.models.sonnet, None);
}

// Model config must survive a config.toml render→parse round-trip, or
// `maybe_rewrite_config_toml` would either drop a hand-set value or thrash the
// file on every reload.
#[test]
fn model_settings_round_trip_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.models = ModelSettings {
        default: Some("opusplan".to_string()),
        opus: Some("claude-opus-4-8[1m]".to_string()),
        sonnet: None,
        haiku: None,
        fable: Some("claude-fable-5".to_string()),
        subagent: Some("claude-haiku-4-5".to_string()),
    };
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.models, profile.models);
}

/// Write `profiles.toml` into the sandboxed home and read it back through the
/// real load boundary — the point of these tests is where normalization
/// happens, so nothing may bypass `load_app_state`.
fn load_state_from_toml(toml: &str) -> AppState {
    std::fs::create_dir_all(clauth_dir().expect("clauth dir")).expect("create clauth dir");
    std::fs::write(app_state_path().expect("state path"), toml).expect("write profiles.toml");
    load_app_state().expect("load state")
}

// A hand-edited out-of-band line must be normalized on LOAD, not on read
// alone: left raw on disk it survives every save and any direct field read
// trusts it. The reset target is the DEFAULT, never the nearest bound —
// honoring a hand-edited 40.0 as 50.0 keeps the weakened gate the edit asked
// for, so fail-safe high instead.
#[test]
fn weekly_switch_threshold_out_of_band_resets_to_default_at_load() {
    let _home = crate::testutil::HomeSandbox::new();
    let low = load_state_from_toml("profiles = []\nweekly_switch_threshold = 40.0\n");
    assert_eq!(
        low.weekly_switch_threshold,
        Some(DEFAULT_WEEKLY_SWITCH_PCT),
        "40.0 resets to the default, never clamps up to MIN"
    );
    let high = load_state_from_toml("profiles = []\nweekly_switch_threshold = 150.0\n");
    assert_eq!(
        high.weekly_switch_threshold,
        Some(DEFAULT_WEEKLY_SWITCH_PCT),
        "150.0 resets to the default, never clamps down to MAX"
    );
}

#[test]
fn weekly_switch_threshold_in_band_survives_load() {
    let _home = crate::testutil::HomeSandbox::new();
    let state = load_state_from_toml("profiles = []\nweekly_switch_threshold = 75.0\n");
    assert_eq!(state.weekly_switch_threshold, Some(75.0));
}

#[test]
fn weekly_switch_threshold_absent_loads_as_default() {
    let _home = crate::testutil::HomeSandbox::new();
    let state = load_state_from_toml("profiles = []\n");
    // Unset stays unset: materializing a value here would start writing the
    // key into every state file that never had it (`skip_serializing_if`).
    assert_eq!(state.weekly_switch_threshold, None);
    assert_eq!(
        state.weekly_switch_threshold_pct(),
        DEFAULT_WEEKLY_SWITCH_PCT
    );
}

// `reload_fingerprint` is the reload trigger for BOTH detectors. These pin the
// three ways it must shift — the profiles.toml mtime (the pre-existing trigger,
// unchanged), a per-account config.toml appearing/vanishing (count), and an
// existing config.toml edited (newest mtime) — plus stability when nothing moved.
#[test]
fn reload_fingerprint_is_stable_with_no_change() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("p"),
    ))
    .expect("save_profile");
    save_app_state(&AppState {
        profiles: vec!["p".into()],
        ..Default::default()
    })
    .expect("save_app_state");
    let first = reload_fingerprint();
    let second = reload_fingerprint();
    assert_eq!(
        first, second,
        "no filesystem change must leave the fingerprint identical"
    );
}

#[test]
fn reload_fingerprint_changes_when_profiles_toml_mtime_bumps() {
    let _home = crate::testutil::HomeSandbox::new();
    save_app_state(&AppState {
        profiles: vec![],
        ..Default::default()
    })
    .expect("save_app_state");
    let before = reload_fingerprint();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
    crate::testutil::set_mtime(&app_state_path().expect("state path"), later);
    let after = reload_fingerprint();
    assert_ne!(
        before, after,
        "a profiles.toml mtime bump must change the fingerprint"
    );
}

/// A codex switch or chain edit writes `codex-profiles.toml` and nothing else,
/// so the fingerprint must move on that file appearing and on its mtime alone
/// — otherwise the TUI and daemon would run on stale codex state forever.
#[test]
fn reload_fingerprint_covers_the_codex_state_file() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");
    let before = reload_fingerprint();
    let path = dir.join("codex-profiles.toml");
    std::fs::write(&path, "profiles = []\n").expect("write codex state");
    let appeared = reload_fingerprint();
    assert_ne!(before, appeared, "the file appearing must shift it");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
    crate::testutil::set_mtime(&path, later);
    assert_ne!(
        appeared,
        reload_fingerprint(),
        "a bare mtime bump must shift it"
    );
}

#[test]
fn reload_fingerprint_bumps_when_a_config_toml_is_added() {
    let _home = crate::testutil::HomeSandbox::new();
    let bare = profiles_root().expect("profiles_root").join("newcomer");
    std::fs::create_dir_all(&bare).expect("mkdir profile");
    let before = reload_fingerprint();
    assert_eq!(
        before
            .config_mtimes
            .iter()
            .find(|(n, _, _)| n == "newcomer")
            .map(|(_, m, _)| m.is_some()),
        Some(false),
        "the dir exists but has no config.toml yet"
    );
    std::fs::write(bare.join("config.toml"), b"auto_start = true\n").expect("write config");
    let after = reload_fingerprint();
    assert_eq!(
        after
            .config_mtimes
            .iter()
            .find(|(n, _, _)| n == "newcomer")
            .map(|(_, m, _)| m.is_some()),
        Some(true),
        "adding a config.toml gives the entry an mtime"
    );
    assert_ne!(before, after);
}

#[test]
fn reload_fingerprint_advances_when_a_config_toml_is_edited() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("p"),
    ))
    .expect("save_profile");
    let cfg = profile_dir(&crate::profile::ProfileName::from("p"))
        .expect("profile_dir")
        .join("config.toml");
    let before = reload_fingerprint();
    let before_mtime = before
        .config_mtimes
        .iter()
        .find(|(n, _, _)| n == "p")
        .and_then(|(_, m, _)| *m);
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
    crate::testutil::set_mtime(&cfg, later);
    let after = reload_fingerprint();
    let after_mtime = after
        .config_mtimes
        .iter()
        .find(|(n, _, _)| n == "p")
        .and_then(|(_, m, _)| *m);
    assert!(
        after_mtime > before_mtime,
        "editing a config.toml must advance its recorded mtime"
    );
    assert_ne!(
        before, after,
        "a config.toml edit must change the fingerprint"
    );
}

#[test]
fn reload_fingerprint_drops_when_a_config_toml_is_removed() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("p"),
    ))
    .expect("save_profile");
    let cfg = profile_dir(&crate::profile::ProfileName::from("p"))
        .expect("profile_dir")
        .join("config.toml");
    let before = reload_fingerprint();
    assert!(
        before
            .config_mtimes
            .iter()
            .any(|(n, m, _)| n == "p" && m.is_some()),
        "the saved profile has a config.toml"
    );
    std::fs::remove_file(&cfg).expect("remove config");
    let after = reload_fingerprint();
    assert!(
        after
            .config_mtimes
            .iter()
            .any(|(n, m, _)| n == "p" && m.is_none()),
        "removing the config.toml drops its recorded mtime to None"
    );
    assert_ne!(before, after);
}

/// Block until a fresh write beside `sidecar` lands a mtime strictly later than
/// `sidecar`'s own, by writing one and reading back what the filesystem stored.
///
/// The two `write_session_token` calls in the tests below are microseconds
/// apart, and a filesystem only stores what its resolution allows: NTFS tied
/// them on a GitHub runner, and any 1 s-granularity mount (HFS+, FAT32, ext3,
/// some NFS) would tie them every run rather than intermittently. Without this
/// the assertions are claims about timestamp resolution, not about the
/// fingerprint.
///
/// Deliberately NOT `testutil::set_mtime`, which is how the sibling tests force
/// a stamp: stamping by hand skips the production write path, so a write that
/// genuinely stopped moving the mtime would still pass. This keeps the real
/// write as the thing under test and only waits for it to be observable. The
/// probe is invisible to `reload_fingerprint`, which stats `config.toml` and
/// `session-token.json` by name and never reads the directory.
fn wait_for_a_distinguishable_mtime(sidecar: &std::path::Path) {
    let after = std::fs::metadata(sidecar)
        .and_then(|m| m.modified())
        .expect("sidecar mtime");
    let probe = sidecar.with_file_name(".mtime-probe");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        std::fs::write(&probe, b"x").expect("probe write");
        let stored = std::fs::metadata(&probe)
            .and_then(|m| m.modified())
            .expect("probe mtime");
        let _ = std::fs::remove_file(&probe);
        if stored > after {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no write got a mtime past {after:?} within 2 s"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

// A `login --setup-token` re-mint writes only `session-token.json` (touches no
// config.toml, no profiles.toml), so the fingerprint must fold that file in or
// the hot reload never sees a new / re-minted long-lived token. What rides is
// when the file was WRITTEN, so every real mint trips it — see the two tests
// below for the timestamp that is not a write, and the write no expiry can see.
#[test]
fn reload_fingerprint_bumps_when_a_session_token_is_added_or_changed() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("p"),
    ))
    .expect("save_profile");
    let minted_at: i64 = 1_700_000_000_000;
    let before = reload_fingerprint();

    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("p"),
        &format!("sk-ant-{}", "m".repeat(40)),
        minted_at,
    )
    .expect("mint a session token");
    let after_add = reload_fingerprint();
    assert_ne!(
        before, after_add,
        "adding a session-token.json must trip the fingerprint"
    );

    wait_for_a_distinguishable_mtime(
        &profile_dir(&crate::profile::ProfileName::from("p"))
            .expect("profile_dir")
            .join("session-token.json"),
    );
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("p"),
        &format!("sk-ant-{}", "r".repeat(40)),
        minted_at + 60 * 60 * 1000,
    )
    .expect("re-mint");
    let after_remint = reload_fingerprint();
    assert_ne!(
        after_add, after_remint,
        "a re-mint is a fresh write of the sidecar and must trip the fingerprint"
    );
}

/// Two writes the sidecar's PARSED contents cannot tell apart, both of which the
/// surfaces reading that file would render differently. A re-mint stamped with
/// the same `expiresAt` (a mint inside the same clock tick, or a hand-edited
/// horizon restored to its old value) changes the bearer and nothing else; an
/// unparseable sidecar appearing is a state the reader reports as "no sidecar"
/// while the operator sees a file. A write time catches both.
#[test]
fn reload_fingerprint_catches_a_sidecar_write_no_expiry_can_see() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("p"),
    ))
    .expect("save_profile");
    let sidecar = profile_dir(&crate::profile::ProfileName::from("p"))
        .expect("profile_dir")
        .join("session-token.json");

    let before_any = reload_fingerprint();
    std::fs::write(&sidecar, b"{}\n").expect("write an unparseable sidecar");
    let after_junk = reload_fingerprint();
    assert_ne!(
        before_any, after_junk,
        "a sidecar that parses to nothing is still a file that appeared"
    );

    let minted_at: i64 = 1_700_000_000_000;
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("p"),
        &format!("sk-ant-{}", "m".repeat(40)),
        minted_at,
    )
    .expect("mint");
    let after_mint = reload_fingerprint();
    wait_for_a_distinguishable_mtime(&sidecar);
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("p"),
        &format!("sk-ant-{}", "r".repeat(40)),
        minted_at,
    )
    .expect("re-mint at the same stamped horizon");
    assert_ne!(
        after_mint,
        reload_fingerprint(),
        "a new bearer under an unchanged expiry is still a re-mint",
    );
}

/// The swap executor stamps the store it repoints to, and for a token-mode member
/// that store IS `session-token.json` (`claude::install_source_path`). Reading
/// that bump as a re-mint forced a full reload of every profile on the next tick.
/// Only a RECEIPTED stamp is discounted — a timestamp that moved with no receipt
/// is indistinguishable from a write and must still trip the reload.
#[test]
fn reload_fingerprint_ignores_a_bare_session_token_stamp() {
    let _home = crate::testutil::HomeSandbox::new();
    // The receipt below is a cache write, gated on the on-disk record.
    crate::testutil::register_names(&["p"]);
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("p"),
    ))
    .expect("save_profile");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("p"),
        &format!("sk-ant-{}", "m".repeat(40)),
        1_700_000_000_000,
    )
    .expect("mint a session token");
    let sidecar = profile_dir(&crate::profile::ProfileName::from("p"))
        .expect("profile_dir")
        .join("session-token.json");
    // Backdated rather than read off the mint, so the re-mint at the bottom
    // CANNOT land on the same `SystemTime`. Both writes are real and both
    // stamp "now": two of them inside one filesystem timestamp tick left the
    // two fingerprints byte-identical at 100ns precision, and the closing
    // `assert_ne!` then read a retired receipt as a live one — 1 run in 3
    // under the full suite. An hour is a value the clock cannot produce here.
    //
    // NOT the sibling tests' `wait_for_a_distinguishable_mtime`, which is the
    // established answer to this same hazard: it waits for a write to land past
    // the path's CURRENT mtime, and by the re-mint below that is the receipted
    // `stamped` half a minute in the FUTURE, so it would wait out its own 2 s
    // deadline and fail. Backdating still leaves the re-mint a real write and
    // the thing under test — a write that stopped moving the mtime leaves the
    // sidecar on this value, which is the one `before` was taken at, so the
    // closing assertion reds exactly as it should.
    let minted = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    crate::testutil::set_mtime(&sidecar, minted);

    let before = reload_fingerprint();
    // What a swap onto this token-mode member leaves behind.
    let stamped = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
    crate::profile_cache::write_touch_receipt(
        &crate::profile::ProfileName::from("p"),
        &sidecar,
        stamped,
        Some(minted),
    );
    crate::testutil::set_mtime(&sidecar, stamped);

    assert_eq!(
        before,
        reload_fingerprint(),
        "a timestamp that moved with no byte behind it is not a config change",
    );

    // And the receipt covers exactly that one stamp: the next real write retires
    // it, so the reload fires again.
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("p"),
        &format!("sk-ant-{}", "r".repeat(40)),
        1_700_000_000_000,
    )
    .expect("re-mint");
    assert_ne!(
        before,
        reload_fingerprint(),
        "a write landing on a stamped sidecar retires the receipt",
    );
}

/// Regression: an edit to a config.toml that is NOT the newest one — its mtime
/// stays below another profile's — must still flip the fingerprint. A max-only
/// fingerprint (count + newest mtime) would miss this (max unchanged, count
/// unchanged, profiles.toml unchanged), silently reintroducing the very
/// "config edit not detected" bug this feature exists to fix.
#[test]
fn reload_fingerprint_catches_a_non_newest_config_edit() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    ))
    .expect("save a");
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("b"),
    ))
    .expect("save b");
    let cfg_a = profile_dir(&crate::profile::ProfileName::from("a"))
        .expect("profile_dir a")
        .join("config.toml");
    let cfg_b = profile_dir(&crate::profile::ProfileName::from("b"))
        .expect("profile_dir b")
        .join("config.toml");
    let base = std::time::SystemTime::now();
    // b stays the newest throughout; a is edited but kept below b.
    crate::testutil::set_mtime(&cfg_b, base + std::time::Duration::from_secs(100));
    crate::testutil::set_mtime(&cfg_a, base + std::time::Duration::from_secs(10));
    let before = reload_fingerprint();
    crate::testutil::set_mtime(&cfg_a, base + std::time::Duration::from_secs(50));
    let after = reload_fingerprint();
    assert_ne!(
        before, after,
        "an edit to a non-newest config.toml must still flip the fingerprint"
    );
}

// The burn-aware tunable accessors reset a hand-edited out-of-band value to the
// default (fail-safe, like the weekly line) and keep an in-band one. An unset
// field reads as the default so `skip_serializing_if` keeps omitting it.
#[test]
fn burn_switch_floor_pct_resets_out_of_band_and_keeps_in_band() {
    let mut st = AppState::default();
    assert_eq!(st.burn_switch_floor_pct(), DEFAULT_BURN_FLOOR_PCT);

    st.burn_switch_floor_pct = Some(MIN_BURN_FLOOR_PCT - 1.0);
    assert_eq!(
        st.burn_switch_floor_pct(),
        DEFAULT_BURN_FLOOR_PCT,
        "below-band floor resets to the default, not clamped to the bound"
    );
    st.burn_switch_floor_pct = Some(99.0);
    assert_eq!(st.burn_switch_floor_pct(), 99.0);
}

// The tests above compare accessors against the constants themselves, so a
// value mutation moves both sides together and nothing reds. Pin the two
// defaults against hardcoded literals so a moved constant fails a named test
// instead of only ever being caught by coincidence (`tui_render_chain.rs`'s
// 98.0 override happens to equal `DEFAULT_WEEKLY_SWITCH_PCT` today).
#[test]
fn default_switch_percentages_are_pinned_at_98() {
    assert_eq!(DEFAULT_WEEKLY_SWITCH_PCT, 98.0);
    assert_eq!(DEFAULT_BURN_FLOOR_PCT, 98.0);
}

#[test]
fn burn_horizon_cap_ms_resets_out_of_band_and_keeps_in_band() {
    let mut st = AppState::default();
    assert_eq!(st.burn_horizon_cap_ms(), DEFAULT_BURN_HORIZON_MS);

    st.burn_horizon_cap_ms = Some(MIN_REFRESH_INTERVAL_MS - 1);
    assert_eq!(st.burn_horizon_cap_ms(), DEFAULT_BURN_HORIZON_MS);
    st.burn_horizon_cap_ms = Some(45_000);
    assert_eq!(st.burn_horizon_cap_ms(), 45_000);
}

#[test]
fn burn_tunables_round_trip_and_omit_when_unset() {
    let on = AppState {
        burn_switch_floor_pct: Some(99.0),
        burn_horizon_cap_ms: Some(45_000),
        ..AppState::default()
    };
    let rendered = toml::to_string_pretty(&on).expect("render");
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse");
    assert_eq!(reparsed.burn_switch_floor_pct, Some(99.0));
    assert_eq!(reparsed.burn_horizon_cap_ms, Some(45_000));

    let off = toml::to_string_pretty(&AppState::default()).expect("render default");
    assert!(
        !off.contains("burn_switch_floor_pct") && !off.contains("burn_horizon_cap_ms"),
        "unset burn tunables must be omitted, got:\n{off}"
    );
}

// The per-account weekly-line override must default unset (follow the chain)
// and round-trip through config.toml. Its LOAD normalization is reset-not-
// clamp, pinned separately by
// `weekly_threshold_out_of_band_resets_to_unset_at_load` — through the real
// disk boundary, where the normalization actually lives.
#[test]
fn weekly_threshold_round_trips_through_config_toml() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert_eq!(cfg.weekly_threshold, None);

    let mut profile = Profile::new("p".to_string(), None, None);
    profile.weekly_threshold = Some(90.0);
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.weekly_threshold, Some(90.0));

    let stock = render_config_toml(&Profile::new("p".to_string(), None, None));
    let parsed: ProfileConfig = toml::from_str(&stock).expect("parse stock toml");
    assert_eq!(parsed.weekly_threshold, None);
}

/// The override RESETS to unset out of band, mirroring the chain-wide line it
/// overrides (never clamps — a `0.98` fraction-vs-percent typo clamped to the
/// band's floor would weekly-block the account from about 1% into its week,
/// chipped as a plausible-looking `WeeklySoft`). Through the real disk
/// boundary: `load_profile` is where the normalization lives.
#[test]
fn weekly_threshold_out_of_band_resets_to_unset_at_load() {
    let _home = HomeSandbox::new();
    let name = "weekly-reset-test";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");

    for (raw, expect, why) in [
        (
            "weekly_threshold = 0.98
",
            None,
            "fraction typo resets",
        ),
        (
            "weekly_threshold = 40.0
",
            None,
            "under-band resets, never clamps to MIN",
        ),
        (
            "weekly_threshold = 150.0
",
            None,
            "over-band resets, never clamps to MAX",
        ),
        (
            "weekly_threshold = nan
",
            None,
            "nan is valid TOML and must not survive",
        ),
        (
            "weekly_threshold = 98.0
",
            Some(98.0),
            "in-band survives",
        ),
    ] {
        std::fs::write(&config_path, raw).expect("write config.toml");
        assert_eq!(
            load_profile(&crate::profile::ProfileName::from(name))
                .expect("load_profile")
                .weekly_threshold,
            expect,
            "{why}: {raw:?}"
        );
    }
}

/// The usage gates default ON through the REAL load boundary — an absent key
/// in a config.toml written before the gates existed keeps stock gating.
/// `profile_config_usage_gates_default_unset` stops at `ProfileConfig` (the
/// unset is `None` there); this pins the `unwrap_or(true)` resolution where
/// it lives, so flipping it to `unwrap_or(false)` cannot stay green.
#[test]
fn usage_gates_default_on_through_the_load_boundary() {
    let _home = HomeSandbox::new();
    let name = "gate-default-test";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");
    std::fs::write(&config_path, "").expect("write empty config.toml");

    let loaded = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert!(loaded.check_weekly, "absent check_weekly loads as ON");
    assert!(loaded.check_scoped, "absent check_scoped loads as ON");

    std::fs::write(
        &config_path,
        "check_weekly = false
check_scoped = false
",
    )
    .expect("write config.toml");
    let loaded = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert!(!loaded.check_weekly, "an explicit false survives the load");
    assert!(!loaded.check_scoped, "an explicit false survives the load");
}

/// The Alibaba console session survives a save/load round trip through
/// `config.toml`'s `[console]` table, and its file keeps the 0600 posture every
/// credential under `~/.clauth` carries.
#[test]
fn a_console_session_round_trips_through_config_toml() {
    let _home = HomeSandbox::new();
    let name = "console-round-trip";
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    profile.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    profile.console = Some(crate::profile::ConsoleCredential {
        token: "console-token-value".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    save_profile(&profile).expect("save_profile");

    let loaded = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    let console = loaded
        .console
        .expect("console session survives the round trip");
    assert_eq!(console.token, "console-token-value");
    assert_eq!(console.site, crate::profile::ConsoleSite::International);
    assert_eq!(console.region, "ap-southeast-1");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
            .expect("config path");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the console token is a credential");
    }
}

/// A `[console]` table with no token is no session at all — a half-filled table
/// (a hand-edit, or a login that never completed) must not reach the fetch layer
/// as a credential. An unset site/region takes the vendor default instead of
/// failing the whole profile load.
#[test]
fn a_console_table_without_a_token_reads_as_no_session() {
    let _home = HomeSandbox::new();
    let name = "console-partial";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");

    std::fs::write(&config_path, "[console]\nsite = \"international\"\n").expect("write");
    assert!(
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .console
            .is_none()
    );

    std::fs::write(&config_path, "[console]\ntoken = \"   \"\n").expect("write");
    assert!(
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .console
            .is_none(),
        "a blank token is the same state as an absent one"
    );

    std::fs::write(&config_path, "[console]\ntoken = \"t\"\n").expect("write");
    let console = load_profile(&crate::profile::ProfileName::from(name))
        .expect("load_profile")
        .console
        .expect("a token alone is a usable session");
    assert_eq!(console.region, "cn-beijing", "the default region");
    assert_eq!(console.site, crate::profile::ConsoleSite::Domestic);
}

/// The `[console]` block ships into every `config.toml`, so its copy is the one
/// place this claim reaches users unprompted — and "the session lasts 48 hours"
/// is false. The 48h runs from the operator's aliyun BROWSER sign-in, not from
/// the `clauth login`: two tokens minted ~4h apart report the same
/// `sessionCreateTimeStamp`/`sessionExpireTimeStamp`, so a fresh login inherits
/// whatever is left and can be worth minutes.
#[test]
fn the_console_template_does_not_promise_a_fresh_48_hours() {
    let rendered = render_config_toml(&Profile::new("p".to_string(), None, None));
    assert!(
        !rendered.contains("lasts 48 hours"),
        "a login does not restart the clock, so the template must not say it does",
    );
    assert!(
        rendered.contains("browser sign-in"),
        "the template has to name what the clock actually runs from",
    );
}

/// The dead-credential record is a new writer under `~/.clauth`, so the
/// tree-wide 0600/0700 invariant covers it — the rule is the TREE, not the
/// secrets in it, and this file holds a hash of a live credential.
#[cfg(unix)]
#[test]
fn the_auth_expired_record_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["perm-auth-record"]);
    crate::profile_cache::write_auth_expired(
        &crate::profile::ProfileName::from("perm-auth-record"),
        0x0123_4567_89ab_cdef,
    );
    assert!(crate::profile_cache::auth_expired_matches(
        &crate::profile::ProfileName::from("perm-auth-record"),
        0x0123_4567_89ab_cdef
    ));
    assert!(
        !crate::profile_cache::auth_expired_matches(
            &crate::profile::ProfileName::from("perm-auth-record"),
            1
        ),
        "a record for another credential is inert"
    );

    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("perm-auth-record"),
        crate::profile_cache::THIRD_PARTY_AUTH_FILE,
    )
    .expect("cache path");
    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "got {mode:#o}");

    crate::profile_cache::clear_auth_expired(&crate::profile::ProfileName::from(
        "perm-auth-record",
    ));
    assert!(!crate::profile_cache::auth_expired_matches(
        &crate::profile::ProfileName::from("perm-auth-record"),
        0x0123_4567_89ab_cdef
    ));
}

/// `save_profile` must not drop non-login blocks (e.g. `mcpOAuth`) that a Claude
/// login-token refresh rewrites over — those are per-MCP-server logins
/// independent of the account. Synthetic tokens only.
#[test]
fn save_profile_preserves_mcp_oauth_across_a_login_refresh() {
    let _home = HomeSandbox::new();

    let mut profile = Profile::new("acct".to_string(), None, None);
    profile.credentials = Some(pair("login-v1", "refresh-v1"));
    save_profile(&profile).expect("save v1");

    // Claude Code authenticates an MCP server, writing an mcpOAuth block into the
    // store file alongside the login.
    let cred_path =
        profile_credentials_path(&crate::profile::ProfileName::from("acct")).expect("cred path");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    stored["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    std::fs::write(&cred_path, serde_json::to_vec(&stored).unwrap()).expect("write mcp block");

    // A Claude login-token rotation re-saves the profile with a new login.
    profile.credentials = Some(pair("login-v2", "refresh-v2"));
    save_profile(&profile).expect("save v2");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read after")).expect("parse");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"], "login-v2",
        "the Claude login rotated to v2"
    );
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the MCP-server login survived the login refresh"
    );
}

/// The crash-recovery leg is the one write that reaches `credentials.json`
/// without going through `save_profile`, so it owes the same preservation. The
/// staged sidecar holds the rotated login alone; writing those bytes raw drops
/// the MCP-server logins the store carries, and the sidecar is consumed right
/// after, so nothing can recover them.
#[test]
fn pending_recovery_preserves_the_stores_mcp_oauth() {
    let _home = HomeSandbox::new();
    let name = "pending-preserve-mcp";
    let committed = pair("old-access", "old-refresh");
    seed_committed(name, &committed);

    // Claude Code authenticated an MCP server through the store.
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    stored["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    std::fs::write(&cred_path, serde_json::to_vec(&stored).unwrap()).expect("write mcp block");

    // A rotation stages, then the commit never lands.
    let staged = pair("new-access", "new-refresh");
    stage_rotated_credentials(&crate::profile::ProfileName::from(name), &staged)
        .expect("stage_rotated_credentials");
    let pending_path = profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&pending_path, now);

    recover_pending_credentials(&crate::profile::ProfileName::from(name), Some(committed));

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("re-read")).expect("parse");
    assert_eq!(
        after["claudeAiOauth"]["refreshToken"], "new-refresh",
        "the adopted rotation still lands"
    );
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the MCP-server login survives an interrupted rotation"
    );
}

#[test]
fn rolling_token_round_trips_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.rolling_token = true;
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert!(parsed.rolling_token);
}

/// The pre-rename `session_feed` spelling is deliberately NOT aliased: no
/// released clauth ever wrote it, and a permanent alias for something that
/// never shipped is pure legacy surface. An unknown key parses as OFF.
#[test]
fn the_pre_rename_session_feed_key_is_not_carried() {
    let legacy: ProfileConfig =
        toml::from_str("session_feed = true\n").expect("parse legacy config");
    assert!(
        !legacy.rolling_token,
        "installs that ran the feature branch re-run `clauth rolling-token <p>` once"
    );
}

/// A test that forgets its sandbox must fail rather than reach the operator's
/// tree: `~/.clauth` is live state a running clauth writes and flocks, so a
/// stray write lands in their accounts and a stray `~/.clauth/.lock` wait times
/// the test out on contention it never staged.
#[test]
fn resolving_a_home_with_no_sandbox_held_panics() {
    // Hold the lock a sandbox holds and set NO override: under a shared-process
    // runner a parallel sandbox would otherwise answer this call.
    let _guard = HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let reached = std::panic::catch_unwind(|| home_dir().map(|p| p.display().to_string()));

    let payload = match reached {
        Ok(home) => panic!("a sandbox-less test resolved a home instead of panicking: {home:?}"),
        Err(payload) => payload,
    };
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        message.contains("HomeSandbox"),
        "the panic must name the fix, got: {message}"
    );
}

/// `login_is_oauth` answers credential typing; its doc must say so rather
/// than claiming the routing question for the managed field. Pinned in
/// source: a doc that reads as the whole routing rule is what routed
/// readers to the wrong answer.
#[test]
fn login_is_oauth_doc_names_the_managed_half_and_points_at_the_routing_answer() {
    let src = include_str!("../../src/profile.rs");
    let before = &src[..src
        .find("pub(crate) fn login_is_oauth(")
        .expect("login_is_oauth is defined")];
    let doc = &before[before
        .rfind("/// Credential typing")
        .expect("the doc opens with its subject")..];
    assert!(
        doc.contains("managed `base_url` field alone"),
        "the doc names the half: {doc}"
    );
    assert!(
        doc.contains("[`stored_endpoint`]"),
        "the doc points at the reader that answers both halves: {doc}"
    );
}

/// `routing_endpoint` reads both halves in the producer's order: an explicit
/// env entry wins over the managed field, and a blank one is no override.
/// Pinned because the blank test is what keeps an empty
/// `ANTHROPIC_BASE_URL` from rerouting a roster row and a cost clause to
/// nothing.
#[test]
fn routing_endpoint_reads_env_first_and_a_blank_entry_is_no_override() {
    let mut p = Profile::new(
        "p".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    );
    assert_eq!(
        p.routing_endpoint(),
        Some("https://api.deepseek.com/anthropic"),
        "the managed field alone answers when no env entry exists"
    );
    p.env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "http://localhost:4000".to_string(),
    );
    assert_eq!(
        p.routing_endpoint(),
        Some("http://localhost:4000"),
        "an explicit env entry wins, the producer's own order"
    );
    p.env
        .insert("ANTHROPIC_BASE_URL".to_string(), "   ".to_string());
    assert_eq!(
        p.routing_endpoint(),
        Some("https://api.deepseek.com/anthropic"),
        "a blank entry is no override"
    );
}
