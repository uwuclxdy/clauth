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

// `preferred_days` must default to empty so every config.toml written before
// the field existed keeps loading with `preferred` alone in charge.
#[test]
fn profile_config_preferred_days_defaults_empty() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert!(cfg.preferred_days.is_empty());
}

// Full names, short forms and mixed case name the same day, and a typo costs
// its own entry instead of the whole profile.
#[test]
fn preferred_days_parse_drops_only_what_it_cannot_read() {
    let raw = vec![
        "Saturday".to_string(),
        "sun".to_string(),
        "funday".to_string(),
        "SAT".to_string(),
    ];
    assert_eq!(
        parse_preferred_days(&raw),
        vec![Weekday::Sat, Weekday::Sun],
        "duplicates collapse and an unparseable entry drops"
    );
}

// The rewrite settles on one spelling instead of alternating with whatever the
// operator typed.
#[test]
fn preferred_days_round_trip_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.preferred_days = vec![Weekday::Sat, Weekday::Sun];
    let rendered = render_config_toml(&profile);
    assert!(
        rendered.contains("preferred_days = [\"sat\", \"sun\"]"),
        "rendered config: {rendered}"
    );
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(
        parse_preferred_days(&parsed.preferred_days),
        vec![Weekday::Sat, Weekday::Sun]
    );
}

// Chain membership does not matter to `is_home_on` — it reads the profile
// list — so the fixture only has to hold the profiles themselves. The
// day-claim axes (disabled, off-chain, the flag) judge a member that holds a
// login; a login-less member reads "logged out" on the same exclusion the walk
// applies, so the fixture holds one by default unless a test set its own.
fn config_of(profiles: Vec<Profile>) -> AppConfig {
    let profiles: Vec<Profile> = profiles
        .into_iter()
        .map(|mut p| {
            if p.credentials.is_none() {
                p.credentials = Some(crate::testutil::oauth_pair(p.name.as_str()));
            }
            p
        })
        .collect();
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    }
}

// A day nobody names leaves `preferred` in charge, so a config that never
// grew a list behaves exactly as it did before the key existed.
#[test]
fn an_unclaimed_day_leaves_the_flag_in_charge() {
    let mut flagged = Profile::new("work".to_string(), None, None);
    flagged.preferred = true;
    let cfg = config_of(vec![flagged]);
    assert!(cfg.is_home_on(&ProfileName::from("work"), Weekday::Mon, &HashSet::new()));
    assert!(cfg.is_home_on(&ProfileName::from("work"), Weekday::Sat, &HashSet::new()));
}

// A list on an account the walk never visits claims nothing. Letting it count
// would stand the flag down on a day its own account can never serve, leaving
// nobody home — the opposite of what the list was written for.
#[test]
fn a_non_members_list_reads_inert() {
    let mut off_chain = Profile::new("personal".to_string(), None, None);
    off_chain.preferred_days = vec![Weekday::Sat];
    let mut flagged = Profile::new("work".to_string(), None, None);
    flagged.preferred = true;
    flagged.credentials = Some(crate::testutil::oauth_pair("work"));
    let cfg = AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("work"), ProfileName::from("personal")],
            fallback_chain: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![flagged, off_chain],
    };

    assert!(
        cfg.is_home_on(&ProfileName::from("work"), Weekday::Sat, &HashSet::new()),
        "an off-chain list does not stand the flag down"
    );
    assert!(!cfg.is_home_on(
        &ProfileName::from("personal"),
        Weekday::Sat,
        &HashSet::new()
    ));
}

// Same for a member the walk skips: disabled here, and auth-broken and
// unresolvable read identically through `walk_excluded`. Its days go back to
// the flag rather than to nobody.
#[test]
fn a_dead_members_list_hands_its_days_back_to_the_flag() {
    let mut dead = Profile::new("personal".to_string(), None, None);
    dead.preferred_days = vec![Weekday::Sat];
    dead.disabled = true;
    let mut flagged = Profile::new("work".to_string(), None, None);
    flagged.preferred = true;
    let cfg = config_of(vec![flagged, dead]);

    assert!(
        cfg.is_home_on(&ProfileName::from("work"), Weekday::Sat, &HashSet::new()),
        "a disabled lister leaves saturday to the flag"
    );
    assert!(
        !cfg.is_home_on(
            &ProfileName::from("personal"),
            Weekday::Sat,
            &HashSet::new()
        ),
        "and cannot be home itself"
    );
}

// A claimed day is claimed against everyone: the weekend account is home on
// Saturday and the flagged one stands down, which is the split an operator
// gets from one line in one profile. Resolving this per profile would leave
// the flag claiming Saturday too, and which of the two won would come down to
// chain order.
#[test]
fn a_listed_day_stands_the_flag_down_elsewhere() {
    let mut weekend = Profile::new("personal".to_string(), None, None);
    weekend.preferred_days = vec![Weekday::Sat, Weekday::Sun];
    let mut flagged = Profile::new("work".to_string(), None, None);
    flagged.preferred = true;
    let cfg = config_of(vec![flagged, weekend]);

    let work = ProfileName::from("work");
    let personal = ProfileName::from("personal");

    assert!(
        cfg.is_home_on(&personal, Weekday::Sat, &HashSet::new()),
        "the list claims sat"
    );
    assert!(
        !cfg.is_home_on(&work, Weekday::Sat, &HashSet::new()),
        "the flag stands down on a claimed day"
    );
    assert!(
        cfg.is_home_on(&work, Weekday::Mon, &HashSet::new()),
        "monday is unclaimed"
    );
    assert!(!cfg.is_home_on(&personal, Weekday::Mon, &HashSet::new()));
}

// One claimant is the ordinary case the whole feature is for, and zero is
// every config that never grew a list — neither is worth a word.
#[test]
fn one_claimant_or_none_raises_no_collision() {
    let mut weekend = Profile::new("personal".to_string(), None, None);
    weekend.preferred_days = vec![Weekday::Sat];
    let flagged = Profile::new("work".to_string(), None, None);
    let cfg = config_of(vec![flagged, weekend]);

    assert_eq!(
        cfg.day_claim_collision(Weekday::Sat, &HashSet::new()),
        None,
        "one claimant"
    );
    assert_eq!(
        cfg.day_claim_collision(Weekday::Mon, &HashSet::new()),
        None,
        "no claimant"
    );
}

// Two lists naming the same day break nothing — the return pass takes the
// first of them that reads clear — but the operator wrote two lines expecting
// one home, so the notice names the day and both claimants.
#[test]
fn two_claimants_raise_a_collision_naming_both() {
    let mut a = Profile::new("work".to_string(), None, None);
    a.preferred_days = vec![Weekday::Sat];
    let mut b = Profile::new("personal".to_string(), None, None);
    b.preferred_days = vec![Weekday::Sat];
    let cfg = config_of(vec![a, b]);

    let notice = cfg
        .day_claim_collision(Weekday::Sat, &HashSet::new())
        .expect("collision");
    assert!(notice.contains("2 accounts claim sat"), "got {notice}");
    assert!(notice.contains("'work'"), "got {notice}");
    assert!(notice.contains("'personal'"), "got {notice}");
}

// A dead account cannot serve the day, so it is not a second claimant — the
// notice would send the operator to fix a collision that `is_home_on` never
// saw. Same `walk_excluded` scan the claim itself runs.
#[test]
fn a_dead_listers_claim_does_not_count_as_a_collision() {
    let mut live = Profile::new("work".to_string(), None, None);
    live.preferred_days = vec![Weekday::Sat];
    let mut dead = Profile::new("personal".to_string(), None, None);
    dead.preferred_days = vec![Weekday::Sat];
    dead.disabled = true;
    let cfg = config_of(vec![live, dead]);

    assert_eq!(cfg.day_claim_collision(Weekday::Sat, &HashSet::new()), None);
}

// The notice is its callers' once-gate key, so it has to be byte-stable while
// nothing changes and different once the day or the claimants do. Without
// this the TUI toast repaints every tick.
#[test]
fn the_collision_notice_is_stable_per_day_and_moves_with_the_claimants() {
    let mut a = Profile::new("work".to_string(), None, None);
    a.preferred_days = vec![Weekday::Sat, Weekday::Sun];
    let mut b = Profile::new("personal".to_string(), None, None);
    b.preferred_days = vec![Weekday::Sat, Weekday::Sun];
    let cfg = config_of(vec![a, b]);

    let sat = cfg
        .day_claim_collision(Weekday::Sat, &HashSet::new())
        .expect("collision");
    assert_eq!(
        cfg.day_claim_collision(Weekday::Sat, &HashSet::new())
            .as_deref(),
        Some(sat.as_str()),
        "the same day re-derives the same bytes"
    );
    assert_ne!(
        cfg.day_claim_collision(Weekday::Sun, &HashSet::new()),
        Some(sat.clone()),
        "the rollover changes it"
    );

    let mut third = Profile::new("spare".to_string(), None, None);
    third.preferred_days = vec![Weekday::Sat];
    third.credentials = Some(crate::testutil::oauth_pair("spare"));
    let mut widened = cfg;
    widened.state.profiles.push(ProfileName::from("spare"));
    widened
        .state
        .fallback_chain
        .push(ProfileName::from("spare"));
    widened.profiles.push(third);
    assert_ne!(
        widened.day_claim_collision(Weekday::Sat, &HashSet::new()),
        Some(sat),
        "a config edit changes it"
    );
}

// The gap the round-2 review found: `walk_excluded` reads an off-chain account
// as eligible, so a healthy non-member with a matching list answered home while
// the lister scan — which walks `fallback_chain` — refused it the same claim.
// `claimed` has to be true for the branch to be reached, so a chain member has
// to name the day as well.
#[test]
fn an_off_chain_list_is_not_home_on_a_day_the_chain_claims() {
    let mut member = Profile::new("work".to_string(), None, None);
    member.preferred_days = vec![Weekday::Sat];
    member.credentials = Some(crate::testutil::oauth_pair("work"));
    let mut off_chain = Profile::new("personal".to_string(), None, None);
    off_chain.preferred_days = vec![Weekday::Sat];

    let cfg = AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("work"), ProfileName::from("personal")],
            fallback_chain: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![member, off_chain],
    };

    assert!(cfg.is_home_on(&ProfileName::from("work"), Weekday::Sat, &HashSet::new()));
    assert!(
        !cfg.is_home_on(
            &ProfileName::from("personal"),
            Weekday::Sat,
            &HashSet::new()
        ),
        "a healthy account off the chain cannot be home on a day it cannot serve"
    );
}

// The flag half had the gap the list half did: an account the walk never
// visits is home on no day, so its projected hop was marking a homecoming that
// cannot happen. Both ways of being unreachable are pinned, since one guard
// answers for both.
#[test]
fn a_flag_on_an_account_the_walk_skips_is_home_on_no_day() {
    let mut disabled = Profile::new("old".to_string(), None, None);
    disabled.preferred = true;
    disabled.disabled = true;
    let cfg = config_of(vec![Profile::new("work".to_string(), None, None), disabled]);
    assert!(
        !cfg.is_home_on(&ProfileName::from("old"), Weekday::Mon, &HashSet::new()),
        "a disabled account carrying the flag is home on no day"
    );

    let mut off_chain = Profile::new("spare".to_string(), None, None);
    off_chain.preferred = true;
    let cfg = AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("work"), ProfileName::from("spare")],
            fallback_chain: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![Profile::new("work".to_string(), None, None), off_chain],
    };
    assert!(
        !cfg.is_home_on(&ProfileName::from("spare"), Weekday::Mon, &HashSet::new()),
        "and neither is one off the chain"
    );
}

// The guard must not cost a healthy account its flag: the day is unclaimed, so
// `preferred` is exactly what should answer.
#[test]
fn a_healthy_members_flag_still_answers_an_unclaimed_day() {
    let mut flagged = Profile::new("work".to_string(), None, None);
    flagged.preferred = true;
    let cfg = config_of(vec![flagged]);
    assert!(cfg.is_home_on(&ProfileName::from("work"), Weekday::Mon, &HashSet::new()));
}

// A list that cannot claim is worth saying at tick time, not just at save
// time: it goes inert later (the account leaves the chain, is disabled, its
// login breaks) and a hand-edited config.toml never passes the editor.
#[test]
fn a_passed_over_lister_names_what_became_of_the_day() {
    let mut carrier = Profile::new("work".to_string(), None, None);
    carrier.preferred_days = vec![Weekday::Sat];
    let mut dead = Profile::new("old".to_string(), None, None);
    dead.preferred_days = vec![Weekday::Sat];
    dead.disabled = true;
    let cfg = config_of(vec![carrier, dead]);

    let notice = cfg
        .day_claim_passed_over(Weekday::Sat, &HashSet::new())
        .expect("a notice");
    assert_eq!(
        notice,
        "sat: the list on 'old' (the account is disabled) cannot claim it. 'work' carries it"
    );
}

// With nobody left to carry it the day is unclaimed, so the flag takes over —
// a different outcome from the carried case and worth wording apart.
#[test]
fn a_passed_over_lister_with_no_carrier_names_the_fallback() {
    let mut dead = Profile::new("old".to_string(), None, None);
    dead.preferred_days = vec![Weekday::Sat];
    dead.disabled = true;
    let cfg = config_of(vec![dead]);

    let notice = cfg
        .day_claim_passed_over(Weekday::Sat, &HashSet::new())
        .expect("a notice");
    assert_eq!(
        notice,
        "sat: the list on 'old' (the account is disabled) cannot claim it. Nothing else claims \
         sat, so `preferred` decides it"
    );
}

// The ordinary case says nothing: every lister could serve, so no line is
// doing anything the operator did not write it to do.
#[test]
fn listers_that_can_all_serve_raise_no_passed_over_notice() {
    let mut a = Profile::new("work".to_string(), None, None);
    a.preferred_days = vec![Weekday::Sat];
    let mut b = Profile::new("personal".to_string(), None, None);
    b.preferred_days = vec![Weekday::Sun];
    let cfg = config_of(vec![a, b]);

    assert_eq!(
        cfg.day_claim_passed_over(Weekday::Sat, &HashSet::new()),
        None
    );
    assert_eq!(
        cfg.day_claim_passed_over(Weekday::Sun, &HashSet::new()),
        None
    );
    assert_eq!(
        cfg.day_claim_passed_over(Weekday::Mon, &HashSet::new()),
        None
    );
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
// `context_nudge_threshold_tokens` defaults to off (`None`), so the key is
// omitted from a stock profiles.toml; a set threshold must round-trip exactly.
#[test]
fn context_nudge_threshold_defaults_off_and_round_trips() {
    let off = AppState::default();
    let rendered_off = toml::to_string_pretty(&off).expect("render default state");
    assert!(
        !rendered_off.contains("context_nudge"),
        "off (default) must be omitted, got:\n{rendered_off}"
    );

    let on = AppState {
        context_nudge_threshold_tokens: Some(600_000),
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains("context_nudge_threshold_tokens = 600000"),
        "on must render explicitly, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert_eq!(reparsed.context_nudge_threshold_tokens, Some(600_000));
}

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

// `walk_order` (issue #86) defaults to `chain` and its on-disk spellings are
// the hyphenated `soonest-weekly-reset` and `most-weekly-headroom` — the serde
// rename is the load boundary, so a rename typo must red here, not on an
// operator's first save. Unset is omitted from a stock file (the
// `reset_display` Option contract), and EVERY value round-trips.
#[test]
fn app_state_walk_order_defaults_chain_and_every_value_round_trips() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert_eq!(state.walk_order(), WalkOrder::Chain);
    assert!(
        state.walk_order.is_none(),
        "unset stays unset, so a stock file omits the key"
    );

    let soonest = AppState {
        walk_order: Some(WalkOrder::SoonestWeeklyReset),
        ..AppState::default()
    };
    let rendered = toml::to_string_pretty(&soonest).expect("render soonest state");
    assert!(
        rendered.contains("walk_order = \"soonest-weekly-reset\""),
        "must render the hyphenated spelling, got:\n{rendered}"
    );
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse soonest state");
    assert_eq!(reparsed.walk_order(), WalkOrder::SoonestWeeklyReset);

    let chain = AppState {
        walk_order: Some(WalkOrder::Chain),
        ..AppState::default()
    };
    let rendered_chain = toml::to_string_pretty(&chain).expect("render chain state");
    assert!(
        rendered_chain.contains("walk_order = \"chain\""),
        "an explicit chain round-trips, got:\n{rendered_chain}"
    );
    let reparsed_chain: AppState = toml::from_str(&rendered_chain).expect("reparse chain state");
    assert_eq!(reparsed_chain.walk_order(), WalkOrder::Chain);

    let headroom: AppState =
        toml::from_str("profiles = []\nwalk_order = \"most-weekly-headroom\"\n")
            .expect("parse the hand-written spelling");
    assert_eq!(headroom.walk_order(), WalkOrder::MostWeeklyHeadroom);
    let rendered_headroom = toml::to_string_pretty(&headroom).expect("render headroom state");
    assert!(
        rendered_headroom.contains("walk_order = \"most-weekly-headroom\""),
        "must render the hyphenated spelling, got:\n{rendered_headroom}"
    );
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

// `preserve_non_login_keys` defaults ON with the same serde contract: a state
// file written before the key existed reads as on, which is what makes the
// preservation reach every host with no config, and an explicit off renders so
// the next load does not turn it back on.
#[test]
fn preserve_non_login_keys_defaults_true_and_an_explicit_off_survives_a_round_trip() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(
        state.preserve_non_login_keys,
        "a state file predating the key must read as on"
    );
    assert!(AppState::default().preserve_non_login_keys);

    let off = AppState {
        preserve_non_login_keys: false,
        ..AppState::default()
    };
    let rendered_off = toml::to_string_pretty(&off).expect("render off state");
    assert!(
        rendered_off.contains("preserve_non_login_keys = false"),
        "off must render explicitly, got:\n{rendered_off}"
    );
    let reparsed: AppState = toml::from_str(&rendered_off).expect("reparse off state");
    assert!(!reparsed.preserve_non_login_keys);

    let rendered_on = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_on.contains("preserve_non_login_keys"),
        "on (default) must be omitted, got:\n{rendered_on}"
    );
}

// `auto_rescue` was the opt-in behind the isolated-transcript rescue, which every
// isolated run now gets unconditionally. `AppState` carries no
// `deny_unknown_fields`, so a profiles.toml written while the key existed still
// loads — the key is ignored rather than refused, and nothing renders it back
// through the model. A save now CARRIES it like any unmodelled key
// (`save_app_state_keeps_unknown_keys_the_file_already_holds`): retired here
// is indistinguishable from future-here, and the file is the record. The load
// half is the one that matters: refusing it would lock an operator out of
// every account on the first launch after an upgrade.
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
        "the model itself never re-emits the key: \n{rendered}"
    );
}

// `save_app_state` rewrites profiles.toml over itself, and the file is shared
// with writers this binary does not model: a newer clauth (the
// `auto_start_queue` erasure, issue #75) or an operator's hand-edit. The key
// belongs to whoever put it in the file; a save that only changes a modelled
// field must keep it.
#[test]
fn save_app_state_keeps_unknown_keys_the_file_already_holds() {
    let _home = HomeSandbox::new();

    let state = AppState {
        profiles: vec![crate::profile::ProfileName::from("holder")],
        ..AppState::default()
    };
    save_app_state(&state).expect("save clean state");

    let path = app_state_path().expect("app_state_path");
    let with_unknown = format!(
        "{}some_unknown_future_key = \"keepme\"\n",
        std::fs::read_to_string(&path).expect("read state file")
    );
    std::fs::write(&path, with_unknown).expect("write state file + unknown key");

    let next = AppState {
        profiles: vec![
            crate::profile::ProfileName::from("holder"),
            crate::profile::ProfileName::from("fixture"),
        ],
        ..AppState::default()
    };
    save_app_state(&next).expect("save state again");

    let after = std::fs::read_to_string(&path).expect("read after");
    assert!(
        after.contains("some_unknown_future_key = \"keepme\""),
        "the unknown key survived the save:\n{after}"
    );
    assert!(
        after.contains("\"fixture\""),
        "the modelled change landed:\n{after}"
    );
}

// A modelled key the new state moved off its on-disk value must take the new
// value — carrying is for keys the model does not hold, never a stale-copy
// resurrection of one it does.
#[test]
fn save_app_state_does_not_resurrect_a_modelled_key_from_disk() {
    let _home = HomeSandbox::new();

    save_app_state(&AppState {
        burn_aware_switching: true,
        ..AppState::default()
    })
    .expect("save with burn_aware on");

    let path = app_state_path().expect("app_state_path");
    std::fs::write(
        &path,
        std::fs::read_to_string(&path).expect("read") + "\nsome_unknown_future_key = \"keepme\"\n",
    )
    .expect("append unknown key");

    save_app_state(&AppState::default()).expect("save burn_aware off");
    let after = std::fs::read_to_string(&path).expect("read after");
    assert!(
        !after.contains("burn_aware_switching = true"),
        "the modelled change is not resurrected from disk:\n{after}"
    );
    assert!(
        after.contains("some_unknown_future_key = \"keepme\""),
        "the unknown key survives:\n{after}"
    );
}

// A future nested key inside a MODELLED table must survive the next save — the
// recursive half of `save_app_state_keeps_unknown_keys_the_file_already_holds`.
// `[update]` rides the top-level carry only while the key is unmodelled; the
// moment the model gains the table (this task), a future sub-key inside it
// would be deleted by every save — the class `[herdr]`/`[serve]` already had.
#[test]
fn save_app_state_keeps_unknown_nested_keys_inside_a_modelled_table() {
    let _home = HomeSandbox::new();

    let mut state = AppState {
        profiles: vec![crate::profile::ProfileName::from("holder")],
        herdr: HerdrSettings {
            pane_tag: false,
            ..HerdrSettings::default()
        },
        serve: ServeSettings {
            session_creation: true,
        },
        ..AppState::default()
    };
    save_app_state(&state).expect("save non-default tables");
    let path = app_state_path().expect("app_state_path");
    let disk = std::fs::read_to_string(&path).expect("read state file");
    // Append future nested keys under the modelled tables, the shape a newer
    // clauth (or a hand-edit) would write.
    std::fs::write(
        &path,
        format!(
            "{disk}[update]\nauto_update = false\nfuture = \"keepme\"\n\
             [herdr.extra]\nwait2 = 7\n[serve.extra]\nwait3 = true\n"
        ),
    )
    .expect("write state file + future nested keys");

    // A save that only adds a profile must keep every future nested key.
    state = toml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse state");
    state
        .profiles
        .push(crate::profile::ProfileName::from("fixture"));
    save_app_state(&state).expect("save again");

    let after = std::fs::read_to_string(&path).expect("read after");
    let parsed: toml::Table = toml::from_str(&after).expect("after parses");
    let nested = |path: &[&str]| {
        let mut cur = &parsed;
        for key in path {
            cur = match cur.get(*key).and_then(toml::Value::as_table) {
                Some(t) => t,
                None => panic!("{key:?} missing from the saved file:\n{after}"),
            };
        }
        cur
    };
    assert_eq!(
        nested(&["herdr", "extra"])
            .get("wait2")
            .and_then(toml::Value::as_integer),
        Some(7),
        "the future nested key under [herdr] survived:\n{after}"
    );
    assert_eq!(
        nested(&["serve", "extra"])
            .get("wait3")
            .and_then(toml::Value::as_bool),
        Some(true),
        "the future nested key under [serve] survived:\n{after}"
    );
    assert_eq!(
        nested(&["update"])
            .get("future")
            .and_then(toml::Value::as_str),
        Some("keepme"),
        "the future nested key under [update] survived:\n{after}"
    );
    assert!(
        after.contains("\"fixture\""),
        "the modelled change landed:\n{after}"
    );
}

// `[update]` defaults ON with the whole table omitted at the default — the
// `preemptive_rotation` serde contract, on a table: a state file written
// before the table existed reads as ON, an explicit OFF serializes, a partial
// or explicitly-ON table loads, and the default renders nothing.
#[test]
fn update_settings_defaults_on_and_an_explicit_off_survives_a_round_trip() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(
        state.update.auto_update,
        "a state file predating the table must read as the new default (on)"
    );
    assert!(AppState::default().update.auto_update);

    let partial: AppState = toml::from_str("profiles = []\n[update]\nauto_update = false\n")
        .expect("parse partial table");
    assert!(
        !partial.update.auto_update,
        "a partial table fills its key from the file"
    );
    let explicit_on: AppState =
        toml::from_str("profiles = []\n[update]\nauto_update = true\n").expect("parse explicit on");
    assert!(explicit_on.update.auto_update);

    let off = AppState {
        update: UpdateSettings { auto_update: false },
        ..AppState::default()
    };
    let rendered_off = toml::to_string_pretty(&off).expect("render off state");
    assert!(
        rendered_off.contains("[update]") && rendered_off.contains("auto_update = false"),
        "off must render explicitly or the next load reverts it to on, got:\n{rendered_off}"
    );
    let reparsed: AppState = toml::from_str(&rendered_off).expect("reparse off state");
    assert!(
        !reparsed.update.auto_update,
        "the operator's off must survive save + reload"
    );

    let rendered_on = toml::to_string_pretty(&AppState::default()).expect("render default state");
    assert!(
        !rendered_on.contains("update"),
        "on (default) must omit the whole [update] table, got:\n{rendered_on}"
    );
}

// The move-to-default edge: a modelled table whose modelled key flips BACK to
// its default renders no `[table]` block on the same save, so a carried
// unknown sub-key has no block to splice into — it must re-attach at EOF
// under a fresh header, never drop (round-2 review, major).
#[test]
fn save_app_state_carries_unknown_nested_keys_when_the_table_renders_default() {
    let _home = HomeSandbox::new();
    let path = app_state_path().expect("app_state_path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        "profiles = []\n\n[update]\nauto_update = false\nfuture = \"keepme\"\n",
    )
    .expect("write disk state");

    let mut state = load_app_state().expect("load");
    assert!(!state.update.auto_update, "the saved off loads");
    // The toggle flips back on: the next render omits the whole [update] table.
    state.update.auto_update = true;
    save_app_state(&state).expect("save");

    let after = std::fs::read_to_string(&path).expect("read after");
    let parsed: toml::Table = toml::from_str(&after).expect("after parses");
    assert_eq!(
        parsed
            .get("update")
            .and_then(|u| u.get("future"))
            .and_then(toml::Value::as_str),
        Some("keepme"),
        "the future key survives the move-to-default save:\n{after}"
    );
    assert!(
        after.contains(PRESERVED_KEYS_MARKER),
        "the carried key sits under the preserved-keys marker:\n{after}"
    );
}

// The disk-side skip (round-3 review, blocker): a disk table holding ONLY an
// unmodelled sub-key parses with the model's table at its default, so the
// round-trip renders no block and classifies nothing — and the top-level carry
// excludes the table because the SAVE renders it (non-default). The sub-key
// must survive via the nested path, classified against the render's own keys.
#[test]
fn save_app_state_carries_an_unknown_subkey_when_the_disk_table_is_all_unmodelled() {
    let _home = HomeSandbox::new();
    let path = app_state_path().expect("app_state_path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "profiles = []\n\n[serve]\nfuture = \"keepme\"\n")
        .expect("write disk state");

    let mut state = load_app_state().expect("load");
    assert!(
        !state.serve.session_creation,
        "the table loads at its default"
    );
    // The save renders [serve] non-default, so the render writes the block.
    state.serve.session_creation = true;
    save_app_state(&state).expect("save");

    let after = std::fs::read_to_string(&path).expect("read after");
    let parsed: toml::Table = toml::from_str(&after).expect("after parses");
    assert_eq!(
        parsed
            .get("serve")
            .and_then(|s| s.get("future"))
            .and_then(toml::Value::as_str),
        Some("keepme"),
        "the sub-key survives the all-unmodelled-disk-table save:\n{after}"
    );
    assert_eq!(
        parsed["serve"]["session_creation"],
        toml::Value::Boolean(true),
        "the modelled change landed:\n{after}"
    );
}

// The config.toml twin of the disk-side skip: a disk `[models]` holding ONLY
// an unmodelled key parses with ModelSettings at its default (every field
// skip-if-none), so the round-trip classifies nothing, and a save whose
// profile HAS a model renders the block — the key must survive.
#[test]
fn save_profile_keeps_an_unknown_subkey_when_the_disk_table_is_all_unmodelled() {
    let _home = HomeSandbox::new();
    let mut profile = Profile::new("cfgnested".to_string(), None, None);
    profile.models.default = Some("claude-opus".to_string());
    save_profile(&profile).expect("save");

    let config_path =
        profile_config_path(&crate::profile::ProfileName::from("cfgnested")).expect("config path");
    // Overwrite the disk file so [models] holds ONLY the unmodelled key: the
    // shape a hand-edit or a newer clauth leaves when the model's own fields
    // are unset in the file.
    std::fs::write(
        &config_path,
        "base_url = \"https://api.example.com\"\n\n[models]\nfuture_model = \"keepme\"\n",
    )
    .expect("write all-unmodelled models table");

    profile.disabled = true;
    save_profile(&profile).expect("save the disable");

    let after = std::fs::read_to_string(&config_path).expect("read after");
    let parsed: toml::Table = toml::from_str(&after).expect("after parses");
    assert_eq!(
        parsed["models"]["future_model"],
        toml::Value::String("keepme".into()),
        "the unmodelled sub-key survives beside the rendered [models] block:\n{after}"
    );
    assert_eq!(
        parsed["models"]["default"],
        toml::Value::String("claude-opus".into()),
        "the modelled change landed:\n{after}"
    );
}

// The splice walk must not read `[`-leading content of a multi-line string as
// a table header: the serializer renders a multi-line value as `key = """` on
// its own line with the closer riding the last content line, so a value like
// `opus = """\na\n[evil] = 1"""` puts a `[`-leading line inside the literal,
// and a false header there fires the leave-block flush INSIDE it — the
// modelled value corrupts and the carried key lands as string text (round-2
// review, minor; doc re-shaped in round 3 to the serializer's measured form).
// Direct unit on the merge, hand-built doc.
#[test]
fn merge_nested_carried_keys_never_reads_a_multiline_string_as_a_header() {
    let rendered = "[models]\nopus = \"\"\"\na\n[evil] = 1\"\"\"\n";
    let nested = vec![(
        "models".to_string(),
        "future".to_string(),
        toml::Value::Integer(7),
    )];
    let out = merge_nested_carried_keys(rendered.to_string(), &nested);
    let parsed: toml::Table = toml::from_str(&out).expect("output parses");
    assert_eq!(
        parsed["models"]["opus"],
        toml::Value::String("a\n[evil] = 1".into()),
        "the modelled value is byte-identical:\n{out}"
    );
    assert_eq!(
        parsed["models"]["future"],
        toml::Value::Integer(7),
        "the carried key lands as a key, not string text:\n{out}"
    );
}

// A dotted sub-header `[t.s]` of the same root is NOT the table's block: the
// walk must end the splice-capable region there, so a pending scalar never
// lands after it (it would scope to `t.s`) — and a table with no bare block
// re-attaches at EOF (round-2 review, nit). Direct unit, hand-built doc.
#[test]
fn merge_nested_carried_keys_treats_a_dotted_subheader_as_end_of_block() {
    let rendered = "[a]\nx = 1\n[t.s]\ny = 2\n";
    let nested = vec![("t".to_string(), "wait".to_string(), toml::Value::Integer(7))];
    let out = merge_nested_carried_keys(rendered.to_string(), &nested);
    let parsed: toml::Table = toml::from_str(&out).expect("output parses");
    assert_eq!(parsed["a"]["x"], toml::Value::Integer(1), "{out}");
    assert_eq!(parsed["t"]["s"]["y"], toml::Value::Integer(2), "{out}");
    assert_eq!(
        parsed["t"]["wait"],
        toml::Value::Integer(7),
        "the scalar scopes to [t], never to [t.s] or [a]:\n{out}"
    );
    assert!(
        out.contains("[t]\nwait = 7"),
        "the no-block table re-attaches at EOF under a fresh header:\n{out}"
    );
}

// The odd-count entry rule breaks on two measured serializer shapes (round-3
// review, major): a single-line BASIC string ending in a `'''` run
// (`key = "a'''"`) and a single-line LITERAL holding `"""` (`'p"""q'`) each
// contain one delimiter occurrence, so line-wise counting read them as
// multi-line openers — the following header vanished into string state and
// the carry re-attached a SECOND table at EOF, a file that no longer parses.
// Direct units on the merge, hand-built docs.
#[test]
fn merge_nested_carried_keys_never_opens_string_state_on_a_single_line_value() {
    for (rendered, value) in [
        (
            "[models]\nopus = \"a'''\"\n\n[herdr]\npane_tag = false\n",
            "a'''",
        ),
        (
            "[models]\ntitle = 'p\"\"\"q'\n\n[herdr]\npane_tag = false\n",
            "p\"\"\"q",
        ),
    ] {
        let nested = vec![(
            "herdr".to_string(),
            "future".to_string(),
            toml::Value::Integer(7),
        )];
        let out = merge_nested_carried_keys(rendered.to_string(), &nested);
        let parsed: toml::Table = toml::from_str(&out).expect("output parses");
        assert_eq!(
            parsed["models"]
                .get("opus")
                .or_else(|| parsed["models"].get("title")),
            Some(&toml::Value::String(value.into())),
            "the modelled value is byte-identical:\n{out}"
        );
        assert_eq!(
            parsed["herdr"]["future"],
            toml::Value::Integer(7),
            "the carried key lands inside the render's own [herdr] block:\n{out}"
        );
    }
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
            ..crate::profile::OAuthToken::default_extra()
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
/// to it (pair present + no usable key AND no env token — an
/// `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` env entry authenticates the
/// spawned `claude`, so the bearer never reaches the endpoint; the same env
/// reading `has_inference_auth` applies on the preserve side). A pure api
/// account with a cleared key keeps its base_url shell so
/// `clear_profile_api_key` stays re-loginable — the same normalize-at-load
/// discipline as `max_auto_spend`, scoped to the leak.
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

    // An env token is the third credential shape the pair cannot leak through:
    // the settings writer applies `profile.env` last, so the spawned claude
    // authenticates with the token and the bearer never reaches the endpoint.
    // The preserve arm counts this shape (`has_inference_auth`), so the load
    // boundary must keep it too, or a preserved endpoint dies at the next load.
    std::fs::write(
        &config_path,
        format!("base_url = \"{endpoint}\"\n\n[env]\nANTHROPIC_AUTH_TOKEN = \"env-bearer\"\n"),
    )
    .expect("write config");
    let env_token = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(
        env_token.base_url.as_deref(),
        Some(endpoint),
        "an env token authenticates the endpoint, so the pair cannot leak"
    );

    // The `ANTHROPIC_API_KEY` env spelling counts the same way.
    std::fs::write(
        &config_path,
        format!("base_url = \"{endpoint}\"\n\n[env]\nANTHROPIC_API_KEY = \"env-key\"\n"),
    )
    .expect("write config");
    let env_key = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(env_key.base_url.as_deref(), Some(endpoint));

    // A whitespace-only env token is no token, the same trim test the
    // preserve side applies — the pair would still reach the endpoint.
    std::fs::write(
        &config_path,
        format!("base_url = \"{endpoint}\"\n\n[env]\nANTHROPIC_AUTH_TOKEN = \"   \"\n"),
    )
    .expect("write config");
    assert_eq!(
        load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile")
            .base_url,
        None,
        "a blank env token is no credential, so the pair would still leak"
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

/// The ruled preserve shape must keep reading the figures the chain feeds:
/// an env-token account's inference spends on the token, and the pair serves
/// usage polling — so the load boundary classifies it OAuth-cache even though
/// the endpoint (and its provider) survive. The third-party leg can never
/// fetch it (`third_party_credentialed` is api-key-only), so classifying it
/// third-party would leave every usage surface reading a cache nothing
/// writes, over the chain-fed figures the ruling assigns it.
#[test]
fn an_env_token_profile_with_a_stored_pair_reads_usage_from_the_chain_cache() {
    let _home = HomeSandbox::new();
    let name = "env-token-usage";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(name),
    ))
    .expect("save_profile");
    let config_path = profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
        .expect("config path");
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let endpoint = "https://api.z.ai/anthropic";
    std::fs::write(
        &config_path,
        format!("base_url = \"{endpoint}\"\n\n[env]\nANTHROPIC_AUTH_TOKEN = \"env-bearer\"\n"),
    )
    .expect("write config");
    std::fs::write(
        &cred_path,
        serde_json::to_string(&oauth_credentials()).expect("ser creds"),
    )
    .expect("write credentials.json");

    let loaded = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert_eq!(
        loaded.base_url.as_deref(),
        Some(endpoint),
        "fixture: the endpoint survives (the env token authenticates it)"
    );
    assert!(
        loaded.provider.is_some(),
        "fixture: the recognised provider derives from the surviving endpoint"
    );
    assert!(
        !loaded.usage_cache_is_third_party(),
        "the chain feeds usage polling for an env-token account, so the \
         figures live in the OAuth cache"
    );
    assert!(
        !stored_usage_cache_is_third_party(&crate::profile::ProfileName::from(name)),
        "the lock-free reader answers the same"
    );

    // The pairless env-token shape stays third-party: with no chain there are
    // no chain figures, so the third-party reading — and its keyless hint — is
    // what remains.
    let _ = std::fs::remove_file(&cred_path);
    let pairless = load_profile(&crate::profile::ProfileName::from(name)).expect("load_profile");
    assert!(
        pairless.usage_cache_is_third_party(),
        "no chain means no chain figures, so the third-party reading (and its \
         keyless hint) is the honest answer"
    );
}

/// The one state the two readers do NOT agree on, pinned in its direction so it
/// stays a known cost rather than a surprise: a pair staged as
/// `credentials.json.pending` and never committed. The lock-free read stats the
/// COMMITTED file only, so it sees no credentials, keeps the `base_url` that a
/// full load would drop (the pair would otherwise reach the endpoint), and
/// answers `true` where `load_profile` answers `false`. Both spellings are
/// pinned in the same direction: with an env token the endpoint survives BOTH
/// reads (the pair never reaches it) and the disagreement is the
/// classification alone — the adopting load reads the chain cache, the stored
/// read still answers third-party.
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

    // The env-token spelling of the same state, in the same direction. Its own
    // name: the first leg's adopt commits `credentials.json`, which would read
    // as "has credentials" and there would be nothing to disagree about.
    let env_name = "endpoint-staged-env";
    save_profile(&crate::testutil::blank_profile(
        &crate::profile::ProfileName::from(env_name),
    ))
    .expect("save env profile");
    let env_config = profile_subpath(&crate::profile::ProfileName::from(env_name), "config.toml")
        .expect("env config path");
    let env_pending = profile_subpath(
        &crate::profile::ProfileName::from(env_name),
        "credentials.json.pending",
    )
    .expect("env pending path");
    std::fs::write(
        &env_config,
        format!("base_url = \"{endpoint}\"\n\n[env]\nANTHROPIC_AUTH_TOKEN = \"env-bearer\"\n"),
    )
    .expect("write env config");
    std::fs::write(
        &env_pending,
        serde_json::to_string(&oauth_credentials()).expect("ser creds"),
    )
    .expect("write env pending sidecar");

    assert!(
        stored_usage_cache_is_third_party(&crate::profile::ProfileName::from(env_name)),
        "the committed file is empty, so this read answers third-party",
    );
    let env_loaded =
        load_profile(&crate::profile::ProfileName::from(env_name)).expect("load_profile");
    assert_eq!(
        env_loaded.base_url.as_deref(),
        Some(endpoint),
        "the env token keeps the endpoint on both readers — only the \
         classification disagrees"
    );
    assert!(
        !env_loaded.usage_cache_is_third_party(),
        "the adopting load reads the chain cache, the same answer the ruled \
         shape gets with a committed pair"
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
            ..crate::profile::OAuthToken::default_extra()
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
        true,
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
        true,
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
            Some(committed),
            true,
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
            None,
            true,
        )),
        Some("only-refresh"),
        "with no commit to compare against, the staged pair is the only live one",
    );
}

/// A stale legacy `.pending` (older than the committed store) is one the RESOLVE
/// half discards: `read_profile` answers with the committed store and neither
/// adopts nor removes the sidecar — a read-only question must not mutate a
/// credential store. `load_profile` keeps its land behaviour for the same shape
/// (pinned by `pending_sidecar_older_than_the_commit_is_discarded_not_reinstalled`).
#[test]
fn read_profile_neither_adopts_nor_removes_a_stale_pending_sidecar() {
    let _home = HomeSandbox::new();
    let name = "pending-read-stale";
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

    let got = read_profile(&crate::profile::ProfileName::from(name)).expect("read_profile");
    assert_eq!(
        refresh_token_of(&got.credentials),
        Some("live-refresh"),
        "a stale sidecar resolves as the committed store, never the superseded pair"
    );
    assert!(
        pending_path.exists(),
        "the read must not remove a stale sidecar; only the load's land leg does"
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
            Some(committed),
            true,
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
            Some(live),
            true,
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
            ..crate::profile::OAuthToken::default_extra()
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
        usage_probe: false,
        usage_probe_interval_ms: crate::profile::DEFAULT_USAGE_PROBE_INTERVAL_MS,
        env: std::collections::BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        preferred_days: Vec::new(),
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        check_credit_cap: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: Some(creds.clone()),
        has_long_lived_sidecar: false,
        unreadable_credentials_store: None,
        setup_token_digest: None,
        provider: None,
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

/// #122: Claude Code writes `chrome/chrome-native-host` 0755 into a runtime
/// tree and points the browser's native-messaging manifest at it, so the
/// browser execs it. The sweep keeps an owner-executable file executable by
/// its owner alone and strips every group and other bit; a file the owner
/// could not execute stays 0600, whatever group or other carried.
#[cfg(unix)]
#[test]
fn the_perms_sweep_keeps_an_executables_owner_exec_bit() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let clauth = clauth_dir().expect("clauth_dir");
    let chrome = clauth
        .join("profiles")
        .join("p")
        .join("runtime-4242-7")
        .join("chrome");
    std::fs::create_dir_all(&chrome).expect("mkdir chrome");
    let chmod = |p: &std::path::Path, mode: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let wrapper = chrome.join("chrome-native-host");
    std::fs::write(&wrapper, b"#!/bin/sh\nexit 7\n").expect("write wrapper");
    chmod(&wrapper, 0o755);
    let manifest = chrome.join("manifest.json");
    std::fs::write(&manifest, b"{}").expect("write manifest");
    chmod(&manifest, 0o644);
    let other_exec = chrome.join("other-exec");
    std::fs::write(&other_exec, b"#!/bin/sh\n").expect("write other-exec");
    chmod(&other_exec, 0o651);
    let open_exec = chrome.join("open-exec");
    std::fs::write(&open_exec, b"#!/bin/sh\n").expect("write open-exec");
    chmod(&open_exec, 0o777);
    let open_plain = chrome.join("open-plain");
    std::fs::write(&open_plain, b"{}").expect("write open-plain");
    chmod(&open_plain, 0o666);
    let open_dir = chrome.join("open-dir");
    std::fs::create_dir(&open_dir).expect("mkdir open-dir");
    chmod(&open_dir, 0o777);

    enforce_clauth_perms(&clauth);

    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&wrapper), 0o700, "the wrapper stays owner-executable");
    assert_eq!(
        mode(&manifest),
        0o600,
        "a plain file is retightened to 0600"
    );
    assert_eq!(
        mode(&other_exec),
        0o600,
        "group and other exec bits never make a file owner-executable"
    );
    assert_eq!(
        mode(&open_exec),
        0o700,
        "an executable loses every group and other bit, write included"
    );
    assert_eq!(
        mode(&open_plain),
        0o600,
        "a plain file loses every group and other bit, write included"
    );
    assert_eq!(mode(&open_dir), 0o700, "a nested open dir is retightened");
    assert_eq!(mode(&chrome), 0o700, "the dir keeps the invariant");

    // The exec the browser performs. A sibling test thread forking while the
    // write fd was open can leave the file text-busy for a moment (ETXTBSY).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        match std::process::Command::new(&wrapper)
            .stdin(std::process::Stdio::null())
            .status()
        {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => break other.expect("exec the wrapper"),
        }
    };
    assert_eq!(status.code(), Some(7), "the swept wrapper still runs");
}

/// The owner-only checker every tree test leans on stays as strict as the rule
/// clauth's own state writers follow: a file is 0600 and a dir 0700, so a
/// state writer that leaves any other mode, an owner exec bit included, is
/// named (the sweep's exec-bit carry-over serves the executables Claude Code
/// writes and the fake-mode tree copy carries in, never a state file).
/// Each of the 512 modes is posed on a file and on an empty dir directly under
/// the root, which pins the per-bit rule there; a 0o755 root, two 0o755
/// non-empty dirs below it and a 0o644 file at the bottom pin that the walk
/// reads the root and descends. A rule that differs only at another depth is
/// not pinned here.
#[cfg(unix)]
#[test]
fn the_owner_only_checker_names_every_mode_but_0600_files_and_0700_dirs() {
    use crate::testutil::owner_only_violations;
    use std::os::unix::fs::PermissionsExt;

    /// Hands every posed dir back to 0o700 on drop, a panic included, so the
    /// tempdir's own removal can read each one.
    struct RestoreDirs(Vec<std::path::PathBuf>);
    impl Drop for RestoreDirs {
        fn drop(&mut self) {
            for dir in &self.0 {
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("tree");
    let nest = root.join("nest");
    let inner = nest.join("inner");
    std::fs::create_dir_all(&inner).expect("mkdir");
    let chmod = |p: &std::path::Path, mode: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let line = |mode: u32, path: &std::path::Path, want: u32| {
        format!("{mode:#o} {} (want {want:#o})", path.display())
    };
    let mut want = Vec::new();
    let mut dirs = RestoreDirs(Vec::new());
    for mode in 0..=0o777u32 {
        let file = root.join(format!("f{mode:03o}"));
        std::fs::write(&file, b"").expect("write");
        chmod(&file, mode);
        if mode != 0o600 {
            want.push(line(mode, &file, 0o600));
        }
        let dir = root.join(format!("d{mode:03o}"));
        std::fs::create_dir(&dir).expect("mkdir");
        dirs.0.push(dir.clone());
        chmod(&dir, mode);
        if mode != 0o700 {
            want.push(line(mode, &dir, 0o700));
        }
    }
    let deep = inner.join("deep");
    std::fs::write(&deep, b"").expect("write");
    chmod(&deep, 0o644);
    want.push(line(0o644, &deep, 0o600));
    chmod(&inner, 0o755);
    want.push(line(0o755, &inner, 0o700));
    chmod(&nest, 0o755);
    want.push(line(0o755, &nest, 0o700));
    chmod(&root, 0o755);
    want.push(line(0o755, &root, 0o700));

    let mut got = owner_only_violations(&root);
    drop(dirs);
    got.sort();
    want.sort();
    // Every file mode but 0o600, every dir mode but 0o700, the nested three and
    // the root.
    assert_eq!(want.len(), 511 + 511 + 4);
    let missing: Vec<_> = want.iter().filter(|l| !got.contains(l)).collect();
    assert!(missing.is_empty(), "the checker missed: {missing:?}");
    let extra: Vec<_> = got.iter().filter(|l| !want.contains(l)).collect();
    assert!(
        extra.is_empty(),
        "the checker named a compliant path: {extra:?}"
    );
    assert_eq!(got, want);
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
        advisor: Some("opus".to_string()),
    };
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.models, profile.models);
    assert!(
        rendered.contains("advisor = \"opus\"\n"),
        "the advisor row is written under [models]: {rendered}"
    );
    assert!(
        rendered.contains(
            "`advisor` is the\n# server-side advisor model (the `advisorModel` setting)."
        ),
        "the generated comment block names the row"
    );
}

#[test]
fn model_settings_advisor_row_is_a_settable_field() {
    let mut models = ModelSettings::default();
    assert!(models.is_empty());
    models.set_field("advisor", Some("opus".to_string()));
    assert_eq!(models.field("advisor").map(String::as_str), Some("opus"));
    assert_eq!(models.advisor.as_deref(), Some("opus"));
    assert!(
        !models.is_empty(),
        "a lone advisor row is not an empty block"
    );
    models.set_field("advisor", None);
    assert!(models.field("advisor").is_none());
    assert!(models.is_empty());
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
    crate::testutil::write_codex_roster(&[]);
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
            .find(|(n, _, _, _)| n == "newcomer")
            .map(|(_, m, _, _)| m.is_some()),
        Some(false),
        "the dir exists but has no config.toml yet"
    );
    std::fs::write(bare.join("config.toml"), b"auto_start = true\n").expect("write config");
    let after = reload_fingerprint();
    assert_eq!(
        after
            .config_mtimes
            .iter()
            .find(|(n, _, _, _)| n == "newcomer")
            .map(|(_, m, _, _)| m.is_some()),
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
        .find(|(n, _, _, _)| n == "p")
        .and_then(|(_, m, _, _)| *m);
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
    crate::testutil::set_mtime(&cfg, later);
    let after = reload_fingerprint();
    let after_mtime = after
        .config_mtimes
        .iter()
        .find(|(n, _, _, _)| n == "p")
        .and_then(|(_, m, _, _)| *m);
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
            .any(|(n, m, _, _)| n == "p" && m.is_some()),
        "the saved profile has a config.toml"
    );
    std::fs::remove_file(&cfg).expect("remove config");
    let after = reload_fingerprint();
    assert!(
        after
            .config_mtimes
            .iter()
            .any(|(n, m, _, _)| n == "p" && m.is_none()),
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

/// An anchor-only write (a re-anchor) moves the reload fingerprint, so a
/// detector picks it up even though no config/credential file changed.
#[test]
fn reload_fingerprint_moves_on_an_anchor_only_write() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(&ProfileName::from("p"))).expect("save p");
    save_app_state(&AppState {
        profiles: vec!["p".into()],
        ..Default::default()
    })
    .expect("app state");
    let before = reload_fingerprint();
    let anchor = profile_dir(&ProfileName::from("p"))
        .expect("profile_dir")
        .join(crate::profile_cache::ACCOUNT_ID_CACHE_FILE);
    std::fs::write(&anchor, "\"uuid-A\"").expect("write the anchor");
    crate::testutil::set_mtime(
        &anchor,
        std::time::SystemTime::now() + std::time::Duration::from_secs(30),
    );
    let after = reload_fingerprint();
    assert_ne!(
        before, after,
        "an anchor-only write must flip the fingerprint"
    );
}

/// A codex `auth.json`-only write moves the reload fingerprint, so a codex
/// in-place rewrite (which touches nothing else) still triggers a reload.
#[test]
fn reload_fingerprint_moves_on_an_auth_json_only_write() {
    let _home = crate::testutil::HomeSandbox::new();
    save_profile(&crate::testutil::blank_profile(&ProfileName::from("p"))).expect("save p");
    let auth = profile_dir(&ProfileName::from("p"))
        .expect("profile_dir")
        .join("auth.json");
    std::fs::write(&auth, r#"{"tokens":{"account_id":"codex-A"}}"#).expect("write auth");
    let before = reload_fingerprint();
    std::fs::write(&auth, r#"{"tokens":{"account_id":"codex-B"}}"#).expect("rewrite auth");
    crate::testutil::set_mtime(
        &auth,
        std::time::SystemTime::now() + std::time::Duration::from_secs(30),
    );
    let after = reload_fingerprint();
    assert_ne!(
        before, after,
        "an auth.json-only write must flip the fingerprint"
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

/// The dead-key record is another writer under `~/.clauth`, holding a hash of a
/// live api key: the tree-wide 0600 rule covers it.
#[cfg(unix)]
#[test]
fn the_dead_key_record_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["perm-dead-key"]);
    let name = crate::profile::ProfileName::from("perm-dead-key");
    crate::profile_cache::write_dead_key(&name, 0x0123_4567_89ab_cdef);
    assert!(crate::profile_cache::dead_key_matches(
        &name,
        0x0123_4567_89ab_cdef
    ));
    assert!(
        !crate::profile_cache::dead_key_matches(&name, 1),
        "a record for another key is inert"
    );

    let path = crate::profile_cache::profile_cache_path(&name, crate::profile_cache::DEAD_KEY_FILE)
        .expect("cache path");
    let mode = std::fs::metadata(&path)
        .expect("the dead-key record exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "got {mode:#o}");
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

/// An OAuth block holding keys `OAuthToken` does not model (`rateLimitTier`,
/// `refreshTokenExpiresAt`, `clientId` — all written by Claude Code through the
/// symlinked store) keeps them through a plain load → mutate → save, the exact
/// shape of every config mutation (`clauth disable`, a TUI toggle): the parse
/// must carry the subkeys into memory and the save must write them back
/// (issue #75).
#[test]
fn save_profile_preserves_unmodelled_claude_ai_oauth_keys() {
    let _home = HomeSandbox::new();

    let mut profile = Profile::new("subkey".to_string(), None, None);
    profile.credentials = Some(pair("login-v1", "refresh-v1"));
    save_profile(&profile).expect("save v1");

    let name = crate::profile::ProfileName::from("subkey");
    let cred_path = profile_credentials_path(&name).expect("cred path");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    stored["claudeAiOauth"]["rateLimitTier"] = serde_json::json!("default_claude_max_5x");
    stored["claudeAiOauth"]["refreshTokenExpiresAt"] = serde_json::json!(1_790_000_000_000_i64);
    stored["claudeAiOauth"]["clientId"] = serde_json::json!("client-abc");
    std::fs::write(&cred_path, serde_json::to_vec(&stored).unwrap()).expect("write subkeys");

    // The config-mutation shape: load (parse), flip a modelled field, save.
    let mut loaded = load_profile(&name).expect("load profile");
    loaded.disabled = true;
    save_profile(&loaded).expect("save the disable");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read after")).expect("parse");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"], "login-v1",
        "the login is untouched"
    );
    assert_eq!(
        after["claudeAiOauth"]["rateLimitTier"], "default_claude_max_5x",
        "rateLimitTier survived the rewrite"
    );
    assert_eq!(
        after["claudeAiOauth"]["refreshTokenExpiresAt"], 1_790_000_000_000_i64,
        "refreshTokenExpiresAt survived the rewrite"
    );
    assert_eq!(
        after["claudeAiOauth"]["clientId"], "client-abc",
        "clientId survived the rewrite"
    );
}

/// A login captured FROM a live file holding unmodelled OAuth subkeys keeps
/// them: the capture parses the live store into `ClaudeCredentials` and saves
/// it into a fresh profile store, so the subkeys must survive the parse — the
/// write-side merge has no disk bytes to merge from on a first capture
/// (issue #75).
#[test]
fn a_captured_login_keeps_the_unmodelled_oauth_keys_it_was_minted_with() {
    let _home = HomeSandbox::new();

    let login: ClaudeCredentials = serde_json::from_value(serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "access-1",
            "refreshToken": "refresh-1",
            "expiresAt": 1_789_000_000_000_i64,
            "scopes": ["user:inference"],
            "subscriptionType": "team",
            "rateLimitTier": "default_claude_max_5x",
            "refreshTokenExpiresAt": 1_790_000_000_000_i64,
            "clientId": "client-abc"
        },
        "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
    }))
    .expect("a live CC store with unmodelled subkeys parses");

    // The capture sink: store the parsed login into a fresh profile.
    let mut profile = Profile::new("caught".to_string(), None, None);
    profile.credentials = Some(login);
    save_profile(&profile).expect("save captured profile");

    let cred_path =
        profile_credentials_path(&crate::profile::ProfileName::from("caught")).expect("cred path");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    let oauth = &stored["claudeAiOauth"];
    assert_eq!(
        oauth["rateLimitTier"], "default_claude_max_5x",
        "rateLimitTier"
    );
    assert_eq!(
        oauth["refreshTokenExpiresAt"], 1_790_000_000_000_i64,
        "refreshTokenExpiresAt"
    );
    assert_eq!(oauth["clientId"], "client-abc", "clientId");
}

/// The parse and the serialize are the two boundaries that can drop an
/// unmodelled key; this pins the round trip through the typed model.
#[test]
fn a_round_trip_through_the_typed_model_keeps_the_extras() {
    let login: ClaudeCredentials = serde_json::from_value(serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "access-1",
            "refreshToken": "refresh-1",
            "rateLimitTier": "default_claude_max_5x",
            "clientId": "client-abc"
        }
    }))
    .expect("parse");
    let round: serde_json::Value =
        serde_json::to_value(&login).expect("serialize the parsed login back");
    assert_eq!(
        round["claudeAiOauth"]["rateLimitTier"],
        "default_claude_max_5x"
    );
    assert_eq!(round["claudeAiOauth"]["clientId"], "client-abc");
}

/// The stamp lands under Claude Code's own key and reads back through the
/// accessor, from a freshly minted block and from a parsed store alike (#78).
#[test]
fn the_rate_limit_tier_stamp_uses_claude_codes_key() {
    let mut login = pair("access", "refresh");
    let oauth = login.claude_ai_oauth.as_mut().expect("oauth");
    assert_eq!(
        oauth.rate_limit_tier(),
        None,
        "a fresh mint carries no tier"
    );
    oauth.set_rate_limit_tier("default_claude_max_5x".to_string());
    assert_eq!(oauth.rate_limit_tier(), Some("default_claude_max_5x"));
    let round: serde_json::Value = serde_json::to_value(&login).expect("serialize");
    assert_eq!(
        round["claudeAiOauth"][RATE_LIMIT_TIER_KEY], "default_claude_max_5x",
        "serialized as a sibling of accessToken, the shape Claude Code writes"
    );

    let parsed: ClaudeCredentials = serde_json::from_value(serde_json::json!({
        "claudeAiOauth": {"accessToken": "a", "rateLimitTier": "default_claude_max_20x"}
    }))
    .expect("parse");
    assert_eq!(
        parsed.claude_ai_oauth.expect("oauth").rate_limit_tier(),
        Some("default_claude_max_20x")
    );
}

/// A login minted by clauth's own browser flow carries no extras, and the
/// serialized store must not grow an empty catch-all key for it.
#[test]
fn a_fresh_login_serializes_with_no_catch_all_key() {
    let login = pair("access", "refresh");
    let round: serde_json::Value = serde_json::to_value(&login).expect("serialize");
    let oauth = &round["claudeAiOauth"];
    let mut keys: Vec<&str> = oauth
        .as_object()
        .expect("oauth object")
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["accessToken", "refreshToken"],
        "a fresh login serializes only its modelled fields, got {oauth}"
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

    recover_pending_credentials(
        &crate::profile::ProfileName::from(name),
        Some(committed),
        true,
    );

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

// ── #80 backfill: the usage poll stamps a missing tier into pre-#80 chains ──
//
// `clauth login` stamps `rateLimitTier` since #80; a chain minted earlier
// carries none. The poll's hourly `/profile` leg backfills it (decision 1 of
// the #80 review), write-if-missing onto the stored chain, under
// the state flock.

/// A stored chain that predates the stamp, polled with the token the `/profile`
/// body answered for: the tier lands under Claude Code's own key. A second
/// call — even with a different polled tier — writes nothing.
#[test]
fn the_poll_backfill_stamps_a_missing_tier_into_the_stored_chain() {
    let _home = HomeSandbox::new();
    let name = "feed-tier";
    crate::testutil::register_names(&[name]);
    seed_committed(name, &pair("at-poll", "rt-poll"));

    let stamped = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-poll",
        "default_claude_max_5x",
    )
    .expect("stamp");
    assert!(stamped, "a matching chain missing the tier is stamped");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    assert_eq!(
        stored["claudeAiOauth"]["rateLimitTier"], "default_claude_max_5x",
        "the tier lands under Claude Code's own key"
    );
    let before = std::fs::read(&cred_path).expect("read store");

    let again = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-poll",
        "default_claude_max_20x",
    )
    .expect("stamp");
    assert!(!again, "a stamped chain is never rewritten");
    assert_eq!(
        std::fs::read(&cred_path).expect("read store"),
        before,
        "the no-op call leaves the store byte-identical"
    );
}

/// The tier is evidence about the exact token the `/profile` body answered for:
/// a chain that moved (a re-login, a concurrent rotation) is never stamped with
/// a reading that belongs to a superseded pair.
#[test]
fn the_poll_backfill_skips_a_chain_the_tier_was_not_fetched_for() {
    let _home = HomeSandbox::new();
    let name = "feed-mismatch";
    crate::testutil::register_names(&[name]);
    seed_committed(name, &pair("at-stored", "rt-stored"));

    let stamped = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-polled",
        "default_claude_max_5x",
    )
    .expect("stamp");
    assert!(
        !stamped,
        "a moved chain is not stamped with a stale reading"
    );
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    assert!(
        stored["claudeAiOauth"].get("rateLimitTier").is_none(),
        "no tier lands on the mismatched chain"
    );
}

/// No OAuth block means nothing to stamp — an api-key profile's store may hold
/// other top-level blocks but no chain — and no `credentials.json` is ever
/// created by the backfill.
#[test]
fn the_poll_backfill_skips_without_an_oauth_chain() {
    let _home = HomeSandbox::new();
    let name = "feed-apikey";
    crate::testutil::register_names(&[name]);

    let stamped = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-poll",
        "default_claude_max_5x",
    )
    .expect("stamp");
    assert!(!stamped, "no store, no stamp");
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    assert!(
        !cred_path.exists(),
        "the backfill never mints a credentials file"
    );

    // A store carrying blocks but no chain (an MCP login alone) is just as
    // un-stampable, and is left byte-identical.
    std::fs::create_dir_all(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir"),
    )
    .expect("mkdir");
    std::fs::write(
        &cred_path,
        r#"{"mcpOAuth":{"linear":{"accessToken":"mock-linear"}}}"#,
    )
    .expect("write store");
    let before = std::fs::read(&cred_path).expect("read store");

    let stamped = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-poll",
        "default_claude_max_5x",
    )
    .expect("stamp");
    assert!(!stamped, "a chain-less store is never stamped");
    assert_eq!(
        std::fs::read(&cred_path).expect("read store"),
        before,
        "the chain-less store is left byte-identical"
    );
}

/// A staged rotation means a commit never landed: writing the pre-rotation
/// pair would move `credentials.json` past the sidecar and get the minted pair
/// discarded by `recover_pending_credentials`. The backfill stands down.
#[test]
fn the_poll_backfill_skips_while_a_rotation_sidecar_is_staged() {
    let _home = HomeSandbox::new();
    let name = "feed-sidecar";
    crate::testutil::register_names(&[name]);
    seed_committed(name, &pair("at-old", "rt-old"));
    stage_rotated_credentials(
        &crate::profile::ProfileName::from(name),
        &pair("at-new", "rt-new"),
    )
    .expect("stage");

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let before = std::fs::read(&cred_path).expect("read store");

    let stamped = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-old",
        "default_claude_max_5x",
    )
    .expect("stamp");
    assert!(!stamped, "a staged rotation stands the backfill down");
    assert_eq!(
        std::fs::read(&cred_path).expect("read store"),
        before,
        "the store is untouched while the sidecar sits"
    );
    assert!(
        profile_subpath(
            &crate::profile::ProfileName::from(name),
            "credentials.json.pending"
        )
        .expect("pending path")
        .exists(),
        "the sidecar is left for recovery, not consumed"
    );
}

/// The tier write goes through the preserving serializer: a top-level block
/// the model does not carry (an MCP-server login) survives the stamp.
#[test]
fn the_poll_backfill_preserves_other_store_blocks() {
    let _home = HomeSandbox::new();
    let name = "feed-preserve";
    crate::testutil::register_names(&[name]);
    seed_committed(name, &pair("at-poll", "rt-poll"));

    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    stored["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    std::fs::write(&cred_path, serde_json::to_vec(&stored).expect("serialize")).expect("write");

    stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-poll",
        "default_claude_max_5x",
    )
    .expect("stamp");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("re-read")).expect("parse");
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the MCP-server login survives the tier stamp"
    );
}

/// A persist leg writes nothing for a profile the roster no longer lists: the
/// poll's work list can lag a concurrent delete/rename by a tick.
#[test]
fn the_poll_backfill_skips_an_unconfigured_profile() {
    let _home = HomeSandbox::new();
    let name = "feed-gone";
    seed_committed(name, &pair("at-poll", "rt-poll"));

    let stamped = stamp_rate_limit_tier_if_missing(
        &crate::profile::ProfileName::from(name),
        "at-poll",
        "default_claude_max_5x",
    )
    .expect("stamp");
    assert!(!stamped, "a profile off the roster is never written");
    let cred_path = profile_subpath(&crate::profile::ProfileName::from(name), "credentials.json")
        .expect("cred path");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cred_path).expect("read store")).expect("parse");
    assert!(
        stored["claudeAiOauth"].get("rateLimitTier").is_none(),
        "no tier lands off-roster"
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

/// `save_profile` rewrites `config.toml` over itself, and the file is shared
/// with writers this binary does not model (a newer clauth, a hand-edit).
/// A save that changes one modelled field must keep the keys it does not know
/// (issue #75, the config.toml sibling of the profiles.toml erasure).
#[test]
fn save_profile_keeps_unknown_config_toml_keys() {
    let _home = HomeSandbox::new();

    let mut profile = Profile::new("cfgkeep".to_string(), None, None);
    profile.fallback_threshold = Some(95.0);
    save_profile(&profile).expect("save");

    let config_path =
        profile_config_path(&crate::profile::ProfileName::from("cfgkeep")).expect("config path");
    let with_unknown = format!(
        "{}\nsome_future_knob = true\n",
        std::fs::read_to_string(&config_path).expect("read config")
    );
    std::fs::write(&config_path, with_unknown).expect("write config + unknown key");

    profile.disabled = true;
    save_profile(&profile).expect("save the disable");

    let after = std::fs::read_to_string(&config_path).expect("read after");
    assert!(
        after.contains("some_future_knob = true"),
        "the unknown key survived the config rewrite:\n{after}"
    );
    assert!(
        after.contains("disabled = true"),
        "the modelled change landed:\n{after}"
    );
}

/// A carried scalar must land at TOP LEVEL, never inside the render's trailing
/// `[env]`/`[models]`/`[console]` table: appended after the header it becomes
/// an env var (wrong type for `env` bricks `load_profile`; a string silently
/// joins the env block and leaks into CC settings). The reviewer round caught
/// the trailing-append shape doing exactly that.
#[test]
fn a_carried_scalar_stays_top_level_beside_a_trailing_table() {
    let _home = HomeSandbox::new();

    let name = crate::profile::ProfileName::from("cfgenv");
    let config_path = profile_config_path(&name).expect("config path");
    std::fs::create_dir_all(config_path.parent().expect("parent")).expect("create profile dir");
    // The knob sits top-level ABOVE [env] — the placement a hand-edit or an
    // older clauth's file has. The test's point is where the SAVE puts it, not
    // where a corrupted file left it.
    std::fs::write(
        &config_path,
        "fallback_threshold = 95.0\n\nsome_future_knob = true\n\n[env]\nHTTP_PROXY = \"http://localhost:8080\"\n",
    )
    .expect("write config");

    let mut loaded = load_profile(&name).expect("load profile");
    loaded.disabled = true;
    save_profile(&loaded).expect("save the disable");

    let after = std::fs::read_to_string(&config_path).expect("read after");
    // must still parse as a profile config, with the knob top-level
    let parsed: ProfileConfig =
        toml::from_str(&after).expect("the carried knob must not corrupt the file");
    assert!(
        !parsed.env.contains_key("some_future_knob"),
        "the carried knob must not land inside [env]:\n{after}"
    );
    let table: toml::Table = after.parse().expect("whole file parses as TOML");
    assert_eq!(
        table.get("some_future_knob"),
        Some(&toml::Value::Boolean(true)),
        "the knob is a top-level key:\n{after}"
    );
}

/// The profiles.toml carry owes the same scoping rule: a scalar carried beside
/// a non-default `[herdr]` block must not land inside it (`HerdrSettings`
/// would silently drop the key on the next load — a loss, not a carry).
#[test]
fn a_carried_profiles_toml_scalar_stays_top_level_beside_herdr() {
    let _home = HomeSandbox::new();

    let state = AppState {
        herdr: HerdrSettings {
            popup_width: crate::profile::PopupWidth::Half,
            ..HerdrSettings::default()
        },
        profiles: vec![crate::profile::ProfileName::from("holder")],
        ..AppState::default()
    };
    save_app_state(&state).expect("save with herdr non-default");

    // The unknown key must be planted TOP-LEVEL, above the rendered [herdr]
    // block — appending after it would nest the key inside [herdr] in the
    // fixture itself, which is the corrupted-file shape, not a valid carry
    // input. A hand-edit or an older clauth writes it top-level.
    let path = app_state_path().expect("app_state_path");
    let raw = std::fs::read_to_string(&path).expect("read");
    let herdr_at = raw.find("[herdr]").expect("herdr block present");
    let planted = format!(
        "{}some_unknown_future_key = \"keepme\"\n\n{}",
        &raw[..herdr_at],
        &raw[herdr_at..]
    );
    std::fs::write(&path, planted).expect("plant unknown key top-level");

    // The second save ALSO renders a [herdr] block (non-default again), so
    // the carried scalar must splice in ABOVE it — the exact shape where a
    // trailing-append carry would nest it inside [herdr].
    save_app_state(&state).expect("save again, herdr still non-default");
    let after = std::fs::read_to_string(&path).expect("read after");
    let table: toml::Table = after.parse().expect("whole file parses");
    assert_eq!(
        table.get("some_unknown_future_key"),
        Some(&toml::Value::String("keepme".into())),
        "the carried key is top-level, not inside [herdr]:\n{after}"
    );
    assert!(
        table
            .get("herdr")
            .and_then(|h| h.get("some_unknown_future_key"))
            .is_none(),
        "the carried key did not nest inside [herdr]:\n{after}"
    );
}

/// A carried table-valued key and a carried scalar in one save: the scalar
/// must not end up inside the carried table's own block either.
#[test]
fn carried_tables_and_scalars_keep_their_own_scopes() {
    let merged = crate::profile::merge_carried_keys(
        "fallback_threshold = 95.0\n".to_string(),
        &[
            (
                "a_table".to_string(),
                toml::Value::Table(
                    [("x".to_string(), toml::Value::Integer(2))]
                        .into_iter()
                        .collect(),
                ),
            ),
            ("z_scalar".to_string(), toml::Value::Boolean(true)),
        ],
    );
    let table: toml::Table = merged.parse().expect("merged doc parses");
    assert_eq!(
        table.get("z_scalar"),
        Some(&toml::Value::Boolean(true)),
        "scalar stays top-level:\n{merged}"
    );
    assert!(
        table.get("a_table").is_some_and(|t| t.get("x").is_some()),
        "table keeps its own sub-keys:\n{merged}"
    );
}

/// Round-2 review hole: a modelled key whose on-disk value EQUALS its
/// `skip_serializing_if` default (`show_pace = false`, a hand-edit writing the
/// default) is erased from the round-trip key set. When the new state then
/// moves the key OFF default, the render emits it AND the disk copy gets
/// carried — a duplicate top-level key, which TOML hard-rejects, bricking
/// profiles.toml. A carried key must be absent from the render as well.
#[test]
fn a_modelled_key_moved_off_default_is_not_carried_beside_its_render() {
    let _home = HomeSandbox::new();

    save_app_state(&AppState::default()).expect("save a default state");
    let path = app_state_path().expect("app_state_path");
    // A hand-edit writes the default explicitly: modelled, but the round-trip
    // of THIS file omits it (its value is the skipped one).
    std::fs::write(
        &path,
        std::fs::read_to_string(&path).expect("read") + "show_pace = false\n",
    )
    .expect("plant an explicit default");

    // The state moves the key off its default, so the render emits it.
    save_app_state(&AppState {
        show_pace: true,
        ..AppState::default()
    })
    .expect("save with show_pace on");

    let after = std::fs::read_to_string(&path).expect("read after");
    let table: toml::Table = after
        .parse()
        .unwrap_or_else(|e| panic!("a duplicate show_pace would fail this parse: {e}\n{after}"));
    assert_eq!(
        table.get("show_pace"),
        Some(&toml::Value::Boolean(true)),
        "exactly one show_pace, the rendered one:\n{after}"
    );
}

/// TOML-valid but `AppState`-invalid disk (a modelled key with a wrong-typed
/// value) reaches the classification refusal — the round-1 finding-4 path —
/// and must carry nothing: the render alone lands, loadable.
#[test]
fn a_state_file_with_a_wrongly_typed_modelled_key_carries_nothing() {
    let _home = HomeSandbox::new();

    let path = app_state_path().expect("app_state_path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    // Valid TOML; `profiles` is not a list, so `AppState` refuses it.
    std::fs::write(
        &path,
        "profiles = \"holder\"\nsome_unknown_future_key = \"keepme\"\n",
    )
    .expect("write unclassifiable state");

    save_app_state(&AppState::default()).expect("save over it");

    let after = std::fs::read_to_string(&path).expect("read after");
    let parsed: AppState = toml::from_str(&after).expect("the written file loads");
    assert!(parsed.profiles.is_empty(), "the render landed whole");
    assert!(
        !after.contains("some_unknown_future_key"),
        "an unclassifiable file carries nothing — duplicating the render's keys would brick it:\n{after}"
    );
}

/// An array-of-tables (`[[key]]`) is not `is_table()`; carried with a scalar it
/// must still land in the tail (after every scalar), or the scalar sorts after
/// the array block and is swallowed into its last element.
#[test]
fn a_carried_array_of_tables_does_not_swallow_a_later_scalar() {
    let merged = crate::profile::merge_carried_keys(
        "fallback_threshold = 95.0\n".to_string(),
        &[
            (
                "a_arr".to_string(),
                toml::Value::Array(vec![toml::Value::Table(
                    [("x".to_string(), toml::Value::Integer(2))]
                        .into_iter()
                        .collect(),
                )]),
            ),
            ("z_scalar".to_string(), toml::Value::Boolean(true)),
        ],
    );
    let table: toml::Table = merged.parse().expect("merged doc parses");
    assert_eq!(
        table.get("z_scalar"),
        Some(&toml::Value::Boolean(true)),
        "scalar stays top-level after the array-of-tables:\n{merged}"
    );
}

/// An unparseable on-disk file must not widen the carry to everything: modelled
/// keys would come back from disk beside the render's copy of the same key,
/// and a duplicate top-level key is a hard parse error — the file this save
/// was supposed to keep loadable. Carrying nothing is the safe direction.
#[test]
fn an_unparseable_state_file_carries_nothing() {
    let _home = HomeSandbox::new();

    let path = app_state_path().expect("app_state_path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "profiles = [\"holder\"\nnot toml at all {{{\n")
        .expect("write unparseable state");

    save_app_state(&AppState {
        profiles: vec![crate::profile::ProfileName::from("holder")],
        ..AppState::default()
    })
    .expect("save over unparseable file");

    let after = std::fs::read_to_string(&path).expect("read after");
    let parsed: AppState = toml::from_str(&after).expect("the written file loads");
    assert_eq!(parsed.profiles.len(), 1, "the render landed whole");
    assert!(
        !after.contains("not toml at all"),
        "nothing is resurrected from an unparseable body — carrying nothing is the safe direction:\n{after}"
    );
    assert!(
        !after.contains(PRESERVED_KEYS_MARKER),
        "no carry marker over an unclassifiable file:\n{after}"
    );
}

/// A plain load must not rewrite `config.toml` just because it holds unknown
/// keys — the drift check compares typed-vs-typed, so unknown keys must not
/// count as drift and must not trigger a write.
#[test]
fn loading_a_config_toml_with_unknown_keys_does_not_rewrite_it() {
    let _home = HomeSandbox::new();

    let name = crate::profile::ProfileName::from("cfgload");
    let config_path = profile_config_path(&name).expect("config path");
    std::fs::create_dir_all(config_path.parent().expect("parent")).expect("create profile dir");
    let body = "fallback_threshold = 95.0\n\nsome_future_knob = true\n";
    std::fs::write(&config_path, body).expect("write config");

    let before = std::fs::read_to_string(&config_path).expect("read before");
    let _profile = load_profile(&name).expect("load");
    let after = std::fs::read_to_string(&config_path).expect("read after");
    assert_eq!(
        before, after,
        "a load rewrites config.toml over unknown keys:\n{after}"
    );
}

/// A non-bare `[env]` key renders as a single-line quoted key that parses back
/// to the same map, one fixture per encoding a key encoder can pick: a
/// TUI-entered `A:B`, a space, a dot (which a bare render reads as a nested
/// table), `=`, a `"` or `\`, both quote kinds, a newline and a control
/// character. A bare-safe key stays bare, byte for byte. Residual: a key class
/// outside these encodings is not exercised.
#[test]
fn an_env_key_that_needs_quotes_renders_quoted_and_round_trips() {
    let mut profile = Profile::new("p".to_string(), None, None);
    for (k, v) in [
        ("A:B", "colon"),
        ("HAS SPACE", "space"),
        ("dot.ted", "dot"),
        ("K=V", "equals"),
        ("say\"hi", "quote"),
        ("back\\slash", "backslash"),
        ("it's \"both\"", "both quotes"),
        ("A\nB", "newline"),
        ("ctl\u{1}", "control"),
        ("PLAIN_KEY-1", "bare"),
    ] {
        profile.env.insert(k.to_string(), v.to_string());
    }
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered)
        .unwrap_or_else(|e| panic!("the render does not parse: {e}\n{rendered}"));
    assert_eq!(
        parsed.env, profile.env,
        "the env map round-trips:\n{rendered}"
    );
    assert!(
        rendered.lines().any(|l| l == "PLAIN_KEY-1 = \"bare\""),
        "a bare-safe key stays bare:\n{rendered}"
    );
}

/// A quoted `[env]` key survives the real write path: a save writes it, a load
/// reads it back, and the file still parses for the load after that.
#[test]
fn a_saved_quoted_env_key_loads_and_reloads() {
    let _home = HomeSandbox::new();

    let mut profile = Profile::new("envquote".to_string(), None, None);
    profile.env.insert("A.B".to_string(), "x".to_string());
    profile.env.insert("A:B".to_string(), "y".to_string());
    save_profile(&profile).expect("save");

    let name = crate::profile::ProfileName::from("envquote");
    let config_path = profile_config_path(&name).expect("config path");
    let on_disk = std::fs::read_to_string(&config_path).expect("read config");
    let parsed: ProfileConfig = toml::from_str(&on_disk)
        .unwrap_or_else(|e| panic!("the saved config.toml does not parse: {e}\n{on_disk}"));
    assert_eq!(parsed.env, profile.env, "{on_disk}");

    let first = load_profile(&name).expect("first load");
    assert_eq!(first.env, profile.env);
    let second = load_profile(&name).expect("second load");
    assert_eq!(second.env, profile.env);
}

/// A hand-quoted key a value encoder would write multi-line survives two
/// loads: the first load must not rewrite the file into one that no longer
/// parses.
#[test]
fn a_hand_quoted_newline_env_key_survives_two_loads() {
    let _home = HomeSandbox::new();
    save_profile(&Profile::new("envnl".to_string(), None, None)).expect("save");
    let name = crate::profile::ProfileName::from("envnl");
    let config_path = profile_config_path(&name).expect("config path");
    std::fs::write(&config_path, "[env]\n\"A\\nB\" = \"x\"\n").expect("write config");
    load_profile(&name).expect("first load");
    let second = load_profile(&name).expect("second load");
    assert_eq!(second.env.get("A\nB").map(String::as_str), Some("x"));
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

// ── [serve] ────────────────────────────────────────────────────────────────

/// `[serve]` round-trips like `[herdr]`: a set key loads, a default state
/// serializes no `[serve]` block, and a partial table fills from the default.
#[test]
fn a_serve_table_round_trips() {
    let _home = HomeSandbox::new();
    let path = app_state_path().expect("app_state_path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "profiles = []\n\n[serve]\nsession_creation = true\n")
        .expect("write profiles.toml");

    let loaded = load_app_state().expect("load");
    assert!(loaded.serve.session_creation, "the [serve] key loads");

    save_app_state(&AppState::default()).expect("save default");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert!(
        !raw.contains("[serve]"),
        "a default [serve] is omitted:\n{raw}"
    );

    std::fs::write(&path, "profiles = []\n\n[serve]\n").expect("partial table");
    let partial = load_app_state().expect("load partial");
    assert!(
        !partial.serve.session_creation,
        "a missing key fills from the default"
    );
}

/// A key inside `[serve]` that `ServeSettings` does not model now SURVIVES the
/// next save while the table renders (its modelled key is non-default) — the
/// recursive carry, the `[update]`-task class fix: a future nested key of a
/// modelled table is carried inside that table's block, exactly like a future
/// top-level key. The default-table case carries the whole table — see the
/// sibling test.
#[test]
fn a_stray_serve_key_survives_while_the_table_renders_non_default() {
    let _home = HomeSandbox::new();
    let path = app_state_path().expect("app_state_path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        "profiles = []\n\n[serve]\nsession_creation = true\nstray = \"gone\"\n",
    )
    .expect("write");

    let state = load_app_state().expect("load");
    assert!(state.serve.session_creation, "the modelled key loads");
    save_app_state(&state).expect("save");

    let after = std::fs::read_to_string(&path).expect("read");
    let parsed: toml::Table = after.parse().expect("whole file parses as TOML");
    assert_eq!(
        parsed.get("serve").and_then(|serve| serve.get("stray")),
        Some(&toml::Value::String("gone".into())),
        "the future nested key survives inside the [serve] table:\n{after}"
    );
    assert!(
        after.contains("session_creation = true"),
        "the modelled key survives:\n{after}"
    );
    assert!(
        after.contains(PRESERVED_KEYS_MARKER),
        "the carried key sits under the preserved-keys marker:\n{after}"
    );
}

/// At its default on disk the whole `[serve]` table is itself unmodelled (the
/// round-trip render omits it), so the carry keeps the table, stray included.
#[test]
fn a_default_serve_table_carries_a_stray_key() {
    let _home = HomeSandbox::new();
    let path = app_state_path().expect("app_state_path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "profiles = []\n\n[serve]\nstray = \"gone\"\n").expect("write");

    let state = load_app_state().expect("load");
    assert!(
        !state.serve.session_creation,
        "the table loads at its default"
    );
    save_app_state(&state).expect("save");

    let after = std::fs::read_to_string(&path).expect("read");
    let parsed: toml::Table = after.parse().expect("whole file parses as TOML");
    assert_eq!(
        parsed.get("serve").and_then(|serve| serve.get("stray")),
        Some(&toml::Value::String("gone".into())),
        "the carried key stays inside the [serve] table:\n{after}"
    );
    assert!(
        parsed.get("stray").is_none(),
        "the carried key must not be hoisted to the top level:\n{after}"
    );
    assert!(
        after.contains(PRESERVED_KEYS_MARKER),
        "the carry sits under the preserved-keys marker:\n{after}"
    );
    let marker = after.find(PRESERVED_KEYS_MARKER).expect("marker present");
    assert!(
        after[marker..].contains("[serve]"),
        "the carried [serve] table lands after the marker:\n{after}"
    );
}

/// A config.toml an older clauth wrote spells `auto_start` as its pre-rename
/// alias `kick_timer`. The round-trip that decides "modelled" parses the alias
/// into `auto_start` and re-emits it under that name, so `kick_timer` reads as
/// unmodelled and was carried beside the render's own `auto_start`. Serde
/// counts an alias and its field as one field, so the next load failed with a
/// duplicate `auto_start` and the profile was unloadable until hand-edited.
#[test]
fn an_aliased_key_on_disk_is_never_carried_beside_its_field() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["alias"]);
    let name = ProfileName::from("alias");
    let mut profile = crate::testutil::blank_profile(&name);
    save_profile(&profile).expect("first save");
    let path = profile_config_path(&name).expect("config path");
    let rendered = std::fs::read_to_string(&path).expect("read");
    // Off, the render writes only the commented `# auto_start = true` hint.
    assert!(!rendered.contains("\nauto_start"), "{rendered}");
    std::fs::write(&path, rendered + "kick_timer = true\n").expect("plant the pre-rename spelling");

    profile.auto_start = true;
    save_profile(&profile).expect("save over the aliased file");

    let after = std::fs::read_to_string(&path).expect("read after");
    let back = load_profile(&name).unwrap_or_else(|e| panic!("reload failed: {e:#}\n{after}"));
    assert!(back.auto_start);
    let table: toml::Table = after.parse().expect("parses");
    assert!(
        !table.contains_key("kick_timer"),
        "the alias is rewritten under its field's name:\n{after}"
    );
}

/// Turning auto-start OFF on a file that spells it `kick_timer = true`: the
/// render leaves `auto_start` at its skipped default, so the alias is not a
/// duplicate there, but carried back it would turn auto-start on again at the
/// next load. The inert rule drops it because it changes a parsed value.
#[test]
fn turning_auto_start_off_never_carries_the_alias_back() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["alias"]);
    let name = ProfileName::from("alias");
    let profile = crate::testutil::blank_profile(&name);
    save_profile(&profile).expect("first save");
    let path = profile_config_path(&name).expect("config path");
    let rendered = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, rendered + "kick_timer = true\n").expect("plant the pre-rename spelling");

    let mut on = load_profile(&name).expect("the alias loads");
    assert!(on.auto_start, "kick_timer = true reads as auto-start on");
    on.auto_start = false;
    save_profile(&on).expect("save auto-start off");

    let after = std::fs::read_to_string(&path).expect("read after");
    let back = load_profile(&name).unwrap_or_else(|e| panic!("reload failed: {e:#}\n{after}"));
    assert!(!back.auto_start, "auto-start stays off:\n{after}");
    let table: toml::Table = after.parse().expect("parses");
    assert!(!table.contains_key("kick_timer"), "{after}");
}

/// A small model with a serde alias at the top level and inside a table, so
/// the inert rule is pinned on its own: neither real model has a nested alias.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AliasModel {
    #[serde(alias = "old")]
    new: bool,
    #[serde(default)]
    inner: AliasInner,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AliasInner {
    #[serde(alias = "old_in")]
    new_in: bool,
}

#[test]
fn an_alias_carry_is_never_inert_and_an_unmodelled_one_is() {
    let render = "new = true\n\n[inner]\nnew_in = true\n";
    let table: toml::Table = render.parse().unwrap();
    let baseline = parsed_as::<AliasModel>(render);
    assert!(baseline.is_some());
    let yes = toml::Value::Boolean(true);

    // Top level: the alias beside its field is a duplicate field.
    assert!(!carry_is_inert::<AliasModel>(
        baseline.as_deref(),
        &table,
        "old",
        &yes
    ));
    assert!(carry_is_inert::<AliasModel>(
        baseline.as_deref(),
        &table,
        "future",
        &yes
    ));
    // A render that does not parse as the model carries nothing.
    assert!(!carry_is_inert::<AliasModel>(None, &table, "future", &yes));

    // Nested, entry by entry: the aliased key goes, its genuine sibling stays.
    let nested = vec![
        ("inner".to_string(), "old_in".to_string(), yes.clone()),
        ("inner".to_string(), "future_in".to_string(), yes.clone()),
    ];
    let merged =
        nested_carry_if_inert::<AliasModel>(render.to_string(), baseline.as_deref(), &nested);
    let back: toml::Table = merged.parse().expect("merged parses");
    let inner = back["inner"].as_table().expect("inner table");
    assert!(inner.contains_key("future_in"), "{merged}");
    assert!(!inner.contains_key("old_in"), "{merged}");
    assert!(toml::from_str::<AliasModel>(&merged).is_ok(), "{merged}");
}

// `bell_command` is a hand-edited key: a rewrite keeps the operator's quoting
// byte for byte.
#[test]
fn app_state_bell_command_round_trips_through_a_save() {
    let _home = HomeSandbox::new();
    let template = "notify-send 'clauth alert' %s";
    save_app_state(&AppState {
        bell_command: Some(template.to_string()),
        ..AppState::default()
    })
    .expect("save state with a bell command");
    assert_eq!(
        load_app_state().expect("reload").bell_command.as_deref(),
        Some(template)
    );
}

// ── the refresh chain's stage, from the loader's side ────────────────────────

use crate::testutil::hold_state_flock;

/// A live `credentials.json.staged` (its base is the store's refresh token, its
/// pair unrecorded) is what a failed rotation persist leaves: `load_profile`
/// must hand back the staged pair, without waiting on the state flock and
/// without consuming the stage, because a loader that writes the pair through
/// raced installs and refused the rotation's own persist.
#[test]
fn a_live_stage_loads_into_memory_without_the_flock_or_a_write() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("stage-live-load");
    seed_committed(name.as_str(), &pair("at-1", "rt-1"));
    let dir = profile_dir(&name).expect("dir");
    // `base` is the hand-computed first 8 bytes of SHA-256("rt-1"), hex.
    std::fs::write(
        dir.join("credentials.json.staged"),
        r#"{"base":"a33d8c625833429d","base_access":"at-1",
            "creds":{"claudeAiOauth":{"accessToken":"at-2","refreshToken":"rt-2"}}}"#,
    )
    .expect("write the stage");
    let _held = hold_state_flock();
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(3)));

    let started = std::time::Instant::now();
    let loaded = load_profile(&name);
    let waited = started.elapsed();
    crate::lock::set_state_lock_timeout_override(None);

    let loaded = loaded.expect("load the profile");
    assert_eq!(
        loaded.refresh_token(),
        Some("rt-2"),
        "the live stage's pair is the chain head and must be what loads"
    );
    assert!(
        waited < std::time::Duration::from_millis(1500),
        "a stage read takes no lock, so a held flock must not delay the load: {waited:?}"
    );
    assert!(
        dir.join("credentials.json.staged").is_file(),
        "a loader never consumes the stage"
    );
    let stored: ClaudeCredentials =
        read_json_file(&dir.join("credentials.json")).expect("read the store");
    assert_eq!(
        stored.refresh_token(),
        Some("rt-1"),
        "a loader never writes the staged pair through"
    );
}

/// A legacy `.pending` newer than the store, read while another process holds
/// the state flock past its deadline: the write-through cannot take the lock,
/// so the sidecar must survive for a later load to land, and this load still
/// hands back the staged pair it judged live.
#[test]
fn a_legacy_pending_survives_a_load_that_cannot_take_the_flock() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("pending-flock-held");
    seed_committed(name.as_str(), &pair("old-access", "old-refresh"));
    let dir = profile_dir(&name).expect("dir");
    let pending_path = dir.join("credentials.json.pending");
    std::fs::write(
        &pending_path,
        serde_json::to_vec(&pair("new-access", "new-refresh")).expect("serialize"),
    )
    .expect("write the legacy sidecar");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(
        &dir.join("credentials.json"),
        now - std::time::Duration::from_secs(60),
    );
    crate::testutil::set_mtime(&pending_path, now);
    let _held = hold_state_flock();
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(200)));

    let got = recover_pending_credentials(&name, Some(pair("old-access", "old-refresh")), true);
    crate::lock::set_state_lock_timeout_override(None);

    assert_eq!(
        refresh_token_of(&got),
        Some("new-refresh"),
        "the sidecar newer than the store is the live pair"
    );
    assert!(
        pending_path.is_file(),
        "a sidecar whose write-through never landed must not be deleted"
    );
}

/// F8: a legacy sidecar OLDER than the store is discarded lock-free, as before
/// the flock rule: a busy flock never holds a load of this profile up, and the
/// stale sidecar still goes.
#[test]
fn a_stale_legacy_pending_goes_without_waiting_on_the_flock() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("pending-stale-flock-held");
    seed_committed(name.as_str(), &pair("old-access", "old-refresh"));
    let dir = profile_dir(&name).expect("dir");
    let pending_path = dir.join("credentials.json.pending");
    std::fs::write(
        &pending_path,
        serde_json::to_vec(&pair("new-access", "new-refresh")).expect("serialize"),
    )
    .expect("write the legacy sidecar");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&pending_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&dir.join("credentials.json"), now);
    let _held = hold_state_flock();
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(200)));

    let got = recover_pending_credentials(&name, Some(pair("old-access", "old-refresh")), true);
    crate::lock::set_state_lock_timeout_override(None);

    assert_eq!(refresh_token_of(&got), Some("old-refresh"));
    assert!(
        !pending_path.exists(),
        "a stale sidecar goes without the flock"
    );
}

/// The no-rewrite read (the MCP handshake) answers an adoptable legacy
/// `.pending` sidecar with the pair the rule judges live — read lock-free and
/// never written through — even while another process holds the state flock:
/// the sidecar's pair comes back at once and both `credentials.json` and the
/// sidecar are left for the next ordinary load to land under the flock.
#[test]
fn the_no_rewrite_read_adopts_a_pending_sidecar_lock_free_and_never_writes() {
    let _home = HomeSandbox::new();
    let name = ProfileName::from("pending-norewrite");
    seed_committed(name.as_str(), &pair("old-access", "old-refresh"));
    let dir = profile_dir(&name).expect("dir");
    let cred_path = dir.join("credentials.json");
    let pending_path = dir.join("credentials.json.pending");
    std::fs::write(
        &pending_path,
        serde_json::to_vec(&pair("new-access", "new-refresh")).expect("serialize"),
    )
    .expect("write the legacy sidecar");
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred_path, now - std::time::Duration::from_secs(60));
    crate::testutil::set_mtime(&pending_path, now);

    let _held = hold_state_flock();
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(2)));

    let started = std::time::Instant::now();
    let profile = read_profile(&name).expect("the no-rewrite read succeeds");
    let waited = started.elapsed();
    crate::lock::set_state_lock_timeout_override(None);

    assert_eq!(
        refresh_token_of(&profile.credentials),
        Some("new-refresh"),
        "the no-rewrite read answers with the sidecar's live pair"
    );
    assert!(
        waited < std::time::Duration::from_secs(1),
        "the no-rewrite read never waits on the state flock (2 s timeout): {waited:?}"
    );
    assert!(
        pending_path.is_file(),
        "the sidecar stays for the next ordinary load to land under the flock"
    );
    let on_disk: ClaudeCredentials = read_json_file(&cred_path).expect("re-read credentials.json");
    assert_eq!(
        on_disk
            .claude_ai_oauth
            .and_then(|o| o.refresh_token)
            .as_deref(),
        Some("old-refresh"),
        "the no-rewrite read never writes the adopted pair through"
    );
}

// The credit gate (#93) defaults ON: an absent key in every config.toml
// written before it existed keeps the cap checked, only `false` renders live,
// and the load boundary turns absence into ON.
#[test]
fn the_credit_gate_defaults_on_and_round_trips_through_config_toml() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert_eq!(cfg.check_credit_cap, None);

    let mut profile = Profile::new("p".to_string(), None, None);
    assert!(
        profile.check_credit_cap,
        "a new profile checks its credit cap"
    );
    profile.check_credit_cap = false;
    let rendered = render_config_toml(&profile);
    assert!(
        rendered.contains("\ncheck_credit_cap = false\n"),
        "{rendered}"
    );
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.check_credit_cap, Some(false));

    let stock = render_config_toml(&Profile::new("p".to_string(), None, None));
    assert!(
        stock.contains(
            "\n\n# Whether auto-switching keeps off this account while its usage-credit cap\n\
             # is spent (Claude Code then refuses credit-billed models such as Fable).\n\
             # Set false to keep it in rotation for other models.\n\
             # check_credit_cap = false\n"
        ),
        "the key carries its own comment block (cloudy, 2026-10-19): {stock}"
    );
    let parsed: ProfileConfig = toml::from_str(&stock).expect("parse stock toml");
    assert_eq!(parsed.check_credit_cap, None);

    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("credit-gate");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let config_path = profile_subpath(&name, "config.toml").expect("config path");
    std::fs::write(&config_path, "").expect("write empty config.toml");
    assert!(
        load_profile(&name).expect("load").check_credit_cap,
        "absent loads as ON"
    );
    std::fs::write(&config_path, "check_credit_cap = false\n").expect("write config.toml");
    assert!(
        !load_profile(&name).expect("load").check_credit_cap,
        "an explicit false survives"
    );
}

/// The MCP handshake's profile read never waits on the state flock, while the
/// ordinary load DOES take it to land an adoptable legacy
/// `credentials.json.pending` sidecar (`land_pending_credentials` writes it
/// through under the flock). The fixture is that sidecar: with a peer holding
/// the flock, `read_profile` answers at once from memory and leaves the sidecar
/// for the next ordinary load, while `load_profile` blocks on the flock to land
/// it.
#[test]
fn the_handshake_profile_read_never_waits_on_the_state_flock() {
    let _home = HomeSandbox::new();
    let clauth = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(clauth.join("profiles").join("held")).expect("create profile dir");
    let config_path = clauth.join("profiles").join("held").join("config.toml");
    std::fs::write(&config_path, "[env]\n\"A:B\" = \"x\"\n").expect("write config.toml");
    // An adoptable legacy `.pending` sidecar (no `credentials.json` to be newer
    // than): an ordinary load takes the state flock and writes it through.
    let pending_path = clauth
        .join("profiles")
        .join("held")
        .join("credentials.json.pending");
    let pending_json =
        "{\"claudeAiOauth\":{\"accessToken\":\"new-access\",\"refreshToken\":\"new-refresh\"}}";
    std::fs::write(&pending_path, pending_json).expect("write the legacy sidecar");

    // This process holds the flock a peer process would hold. `try_lock` so a
    // second holder can never hang the suite; a lock file is never truncated.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(clauth.join(".lock"))
        .expect("open the state lock");
    lock.try_lock().expect("take the state flock");
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(2)));
    let name = ProfileName::from("held");

    let started = std::time::Instant::now();
    let profile = read_profile(&name).expect("the no-rewrite read succeeds");
    let waited = started.elapsed();
    assert_eq!(
        profile.env.get("A:B").map(String::as_str),
        Some("x"),
        "the no-rewrite read still reads the profile"
    );
    assert!(
        waited < std::time::Duration::from_secs(1),
        "the handshake read never waits on the state flock (2 s timeout): {waited:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&config_path).expect("read back"),
        "[env]\n\"A:B\" = \"x\"\n",
        "the no-rewrite read leaves the file as it is"
    );
    assert_eq!(
        std::fs::read_to_string(&pending_path).expect("read back the sidecar"),
        pending_json,
        "the no-rewrite read leaves the sidecar for the next ordinary load"
    );

    // The control: the ordinary load takes the flock to land the same sidecar,
    // so the test would fail if the fixture stopped being adoptable.
    let (tx, rx) = std::sync::mpsc::channel();
    let control_name = name.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let outcome = load_profile(&control_name);
        let _ = tx.send((started.elapsed(), outcome.is_ok()));
    });
    let (control_waited, ok) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the control load returns after the scaled flock timeout");
    assert!(ok, "the control load still succeeds");
    assert!(
        control_waited >= std::time::Duration::from_millis(1500),
        "the control: load_profile waits on the flock: {control_waited:?}"
    );
    drop(lock);
}

// ---- `stored_login` (the on-disk drain read) ----

/// A `credentials.json` that exists but cannot be parsed proves nothing: the
/// read is unknown — a store that exists but cannot be read or parsed — never
/// "no login".
#[test]
fn stored_login_reads_a_torn_credentials_file_as_unknown() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-creds");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let dir = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::write(
        dir.join("credentials.json"),
        "{\"claudeAiOauth\": {\"accessToken\": ",
    )
    .expect("write torn credentials.json");

    assert_eq!(
        crate::profile::stored_login(&name),
        crate::profile::StoredLogin::Unknown,
        "an unreadable credentials.json must not read as no login"
    );
}

/// A `config.toml` that exists but cannot be parsed proves nothing: the read
/// is unknown — a store that exists but cannot be read or parsed — never "no
/// key".
#[test]
fn stored_login_reads_a_torn_config_as_unknown() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-config");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let dir = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::write(
        dir.join("config.toml"),
        "api_key = \"sk-test\"\nbase_url = [",
    )
    .expect("write torn config.toml");

    assert_eq!(
        crate::profile::stored_login(&name),
        crate::profile::StoredLogin::Unknown,
        "an unreadable config.toml must not read as no login"
    );
}

/// A `session-token.json` that exists but cannot be parsed proves nothing: the
/// read is unknown, never "no sidecar" (`session_token_status` disengages on a
/// corrupt sidecar, which is right for installing, wrong for proving absence).
#[test]
fn stored_login_reads_a_torn_sidecar_as_unknown() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-sidecar");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let dir = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::write(
        dir.join("session-token.json"),
        "{\"claudeAiOauth\": {\"accessToken\": ",
    )
    .expect("write torn session-token.json");

    assert_eq!(
        crate::profile::stored_login(&name),
        crate::profile::StoredLogin::Unknown,
        "an unreadable session-token.json must not read as no login"
    );
}

/// Any readable fact proving a login wins, whatever else is unreadable: a
/// `config.toml` with a usable api key holds a login even beside a torn
/// `credentials.json`.
#[test]
fn stored_login_reads_a_torn_credentials_file_beside_a_readable_api_key_as_holding_a_login() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-beside-key");
    let mut keyed = crate::testutil::blank_profile(&name);
    keyed.api_key = Some("sk-test".to_string());
    save_profile(&keyed).expect("save_profile");
    let dir = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::write(
        dir.join("credentials.json"),
        "{\"claudeAiOauth\": {\"accessToken\": ",
    )
    .expect("write torn credentials.json");

    assert_eq!(
        crate::profile::stored_login(&name),
        crate::profile::StoredLogin::HoldsLogin,
        "a readable api key holds a login even beside an unreadable credentials.json"
    );
}

/// With no store files at all, the read proves none: every store that exists is
/// readable and holds nothing.
#[test]
fn stored_login_reads_no_files_as_holding_none() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("no-files");

    assert_eq!(
        crate::profile::stored_login(&name),
        crate::profile::StoredLogin::HoldsNone,
        "no store files prove no login"
    );
}

// ---- a torn `credentials.json` at load ----

const TORN_STORE: &str = "{\"claudeAiOauth\": {\"accessToken\": ";

fn tear_store(name: &crate::profile::ProfileName) -> std::path::PathBuf {
    let path = crate::profile::profile_dir(name)
        .expect("profile dir")
        .join("credentials.json");
    std::fs::write(&path, TORN_STORE).expect("write torn credentials.json");
    path
}

/// One unparseable store costs its own account its login and nothing else: the
/// roster loads, the torn account keeps its settings and reads logged out, and
/// the torn bytes stay on disk for a later read or a re-login.
#[test]
fn a_torn_store_loads_its_account_logged_out_beside_the_rest() {
    let _home = HomeSandbox::new();
    let torn = crate::profile::ProfileName::from("torn");
    let fine = crate::profile::ProfileName::from("fine");
    let mut torn_profile = crate::testutil::blank_profile(&torn);
    torn_profile.fallback_threshold = Some(80.0);
    torn_profile.credentials = Some(crate::testutil::oauth_pair("torn-access"));
    save_profile(&torn_profile).expect("save torn");
    let mut fine_profile = crate::testutil::blank_profile(&fine);
    fine_profile.credentials = Some(crate::testutil::oauth_pair("fine-access"));
    save_profile(&fine_profile).expect("save fine");
    save_app_state(&AppState {
        profiles: vec![torn.clone(), fine.clone()],
        ..AppState::default()
    })
    .expect("save state");
    let store = tear_store(&torn);

    let config = load_config().expect("one torn store must not fail the roster's load");

    assert_eq!(config.profiles.len(), 2, "both accounts load");
    let torn_loaded = config.find(&torn).expect("the torn account loads");
    assert!(
        torn_loaded.credentials.is_none(),
        "the torn account reads logged out"
    );
    assert!(!torn_loaded.holds_login(), "a torn store proves no login");
    assert_eq!(
        torn_loaded.fallback_threshold,
        Some(80.0),
        "the torn account keeps its config"
    );
    assert_eq!(
        config.find(&fine).and_then(|p| p.access_token()),
        Some("fine-access"),
        "the other account keeps its login"
    );
    assert_eq!(
        std::fs::read_to_string(&store).expect("read torn store"),
        TORN_STORE,
        "the load leaves the torn bytes as they were"
    );

    let without_rewrite = load_config_without_rewrite()
        .expect("the handshake's no-rewrite load must not fail on a torn store either");
    assert_eq!(without_rewrite.profiles.len(), 2);
}

/// An OAuth account whose `config.toml` also names an endpoint (and carries no
/// key of its own) loads with that endpoint dropped, so its bearer never goes
/// there. A torn store is a store that exists, so the endpoint stays dropped:
/// the load must agree with the lock-free endpoint readers, which judge by the
/// file existing.
#[test]
fn a_torn_store_keeps_an_oauth_accounts_endpoint_dropped() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("hybrid");
    let mut hybrid = crate::testutil::blank_profile(&name);
    hybrid.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    hybrid.credentials = Some(crate::testutil::oauth_pair("hybrid-access"));
    save_profile(&hybrid).expect("save hybrid");
    assert_eq!(
        load_profile(&name).expect("intact load").base_url,
        None,
        "control: an intact store drops the endpoint"
    );
    tear_store(&name);

    let loaded = load_profile(&name).expect("a torn store loads");

    assert_eq!(
        loaded.base_url, None,
        "a torn store drops the endpoint as an intact one does"
    );
    assert_eq!(loaded.provider, None);
    assert!(
        matches!(
            crate::profile::stored_endpoint(&name),
            crate::profile::StoredEndpoint::Anthropic
        ),
        "the lock-free endpoint reader agrees"
    );
}

/// An account that runs on its own api key records the same spawn identity
/// whether its OAuth store reads or not: the identity of a keyed account never
/// reads the login, so a torn store must not cost a delegate's speed sample.
#[test]
fn a_torn_store_keeps_a_keyed_accounts_spawn_identity() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-spawn");
    let mut keyed = crate::testutil::blank_profile(&name);
    keyed.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    keyed.api_key = Some("sk-torn-spawn".to_string());
    keyed.credentials = Some(crate::testutil::oauth_pair("torn-spawn-access"));
    save_profile(&keyed).expect("save keyed");
    let intact = crate::usage::throughput_identity_for_spawn(
        &name,
        &load_profile(&name).expect("intact load"),
    );
    assert!(
        intact.is_some(),
        "control: an intact store records an identity"
    );
    tear_store(&name);

    let loaded = load_profile(&name).expect("a torn store loads");

    assert_eq!(
        crate::usage::throughput_identity_for_spawn(&name, &loaded),
        intact,
        "a torn store leaves a keyed account's spawn identity as it was"
    );
}

/// A store that cannot be read at all (here a directory in its place) loads
/// logged out like a torn one, and the account carries the read error the
/// notice names instead of a parse failure.
#[test]
fn an_unreadable_store_loads_logged_out_and_names_the_os_error() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("p1");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save p1");
    let store = crate::profile::profile_dir(&name)
        .expect("profile dir")
        .join("credentials.json");
    std::fs::create_dir(&store).expect("a directory in the store's place");

    let loaded = load_profile(&name).expect("an unreadable store loads");

    assert!(loaded.credentials.is_none(), "it reads logged out");
    // The OS renders the read error (`Is a directory (os error 21)` on unix,
    // `Access is denied. (os error 5)` on windows): read it off the same path.
    let os_error = crate::testutil::directory_read_error(&store);
    assert_eq!(
        crate::profile::unreadable_store_notice(&loaded),
        Some(format!(
            "clauth: p1: its stored login could not be read (credentials.json: {os_error}), so it shows as logged out until `clauth login p1`"
        )),
    );
    assert!(store.is_dir(), "the load leaves it as it was");
}

/// The notice for an unparseable store names the parse failure, and a loaded
/// account with a readable store has none.
#[test]
fn the_unreadable_store_notice_names_a_parse_failure() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("p1");
    let mut p1 = crate::testutil::blank_profile(&name);
    p1.credentials = Some(crate::testutil::oauth_pair("p1-access"));
    save_profile(&p1).expect("save p1");
    assert_eq!(
        crate::profile::unreadable_store_notice(&load_profile(&name).expect("intact")),
        None,
        "a readable store has no notice"
    );
    tear_store(&name);

    assert_eq!(
        crate::profile::unreadable_store_notice(&load_profile(&name).expect("torn")).as_deref(),
        Some(
            "clauth: p1: its stored login could not be read (credentials.json does not parse), so it shows as logged out until `clauth login p1`"
        ),
    );
}

/// A store truncated inside a multi-byte character is a content tear, not an
/// OS read failure: the notice names a parse failure, not std's fixed
/// "stream did not contain valid UTF-8".
#[test]
fn a_store_cut_mid_utf8_names_a_parse_failure() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("p1");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save p1");
    let store = crate::profile::profile_dir(&name)
        .expect("profile dir")
        .join("credentials.json");
    // Cut inside the two-byte UTF-8 sequence for é (0xC3 0xA9): the file ends
    // mid-codepoint, so `read_to_string` fails with `InvalidData`.
    std::fs::write(&store, b"{\"claudeAiOauth\": {\"accessToken\": \"\xC3")
        .expect("write mid-UTF-8 store");

    let loaded = load_profile(&name).expect("a mid-UTF-8 store loads logged out");

    assert!(loaded.credentials.is_none(), "it reads logged out");
    assert_eq!(
        crate::profile::unreadable_store_notice(&loaded).as_deref(),
        Some(
            "clauth: p1: its stored login could not be read (credentials.json does not parse), so it shows as logged out until `clauth login p1`"
        ),
        "a mid-codepoint cut is a parse failure, not an OS read error"
    );
}

/// The torn-store notice prints at most once per account per process, however
/// many times the roster loads, and only while the CLI dispatch has enabled it.
#[test]
fn a_torn_store_notice_prints_once_per_account_per_process() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-notice-once");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    save_app_state(&AppState {
        profiles: vec![name.clone()],
        ..AppState::default()
    })
    .expect("save state");
    let _store = tear_store(&name);

    let capture = crate::out::ErrCapture::new();
    let guard = capture.capture_here();
    let _armed = crate::profile::TornStoreNoticeArmed::new();

    load_config().expect("first load");
    load_config().expect("second load");

    drop(guard);

    let expected = format!(
        "clauth: {name}: its stored login could not be read (credentials.json does not parse), so it shows as logged out until `clauth login {name}`\n"
    );
    assert_eq!(
        String::from_utf8(capture.snapshot()).expect("utf-8 stderr"),
        expected,
        "two roster loads print the notice exactly once"
    );
}

/// The notice is off by default: a roster load no CLI dispatch armed prints
/// nothing, so the TUI's alternate screen, the daemon's log and a machine
/// channel never carry it.
#[test]
fn a_torn_store_load_without_the_notice_armed_prints_nothing() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-notice-off");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    save_app_state(&AppState {
        profiles: vec![name.clone()],
        ..AppState::default()
    })
    .expect("save state");
    let _store = tear_store(&name);

    let capture = crate::out::ErrCapture::new();
    let guard = capture.capture_here();

    load_config().expect("an un-armed load still loads");

    drop(guard);

    assert!(
        capture.snapshot().is_empty(),
        "an un-armed load prints nothing: {:?}",
        String::from_utf8_lossy(&capture.snapshot())
    );
}

/// A legacy `.pending` sidecar beside a torn store still adopts: the load
/// repairs `credentials.json` from the sidecar, and the account reads with the
/// pair, not as torn.
#[test]
fn a_pending_sidecar_beside_a_torn_store_adopts_and_is_not_flagged() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("torn-adopt");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let store = tear_store(&name);
    let pending = profile_subpath(&name, "credentials.json.pending").expect("pending path");
    std::fs::write(
        &pending,
        serde_json::to_string(&oauth_credentials()).expect("ser creds"),
    )
    .expect("write pending sidecar");

    let loaded = load_profile(&name).expect("a torn store beside a pending sidecar loads");

    assert_eq!(
        loaded.access_token(),
        Some("tok-access"),
        "the adoptable sidecar supplies the pair"
    );
    assert!(
        loaded.unreadable_credentials_store.is_none(),
        "an adopted pair is a login, not a torn store"
    );
    assert_ne!(
        std::fs::read_to_string(&store).expect("read store"),
        TORN_STORE,
        "the adopt repairs the committed store"
    );
}

/// A torn `config.toml` line would otherwise quote the api key in the parser's
/// own message; the load renders the position alone, so the key never reaches
/// `{e:?}`/`{e:#}` on any surface.
#[test]
fn a_torn_config_toml_names_the_position_and_never_the_secret() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("c2-torn");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let config_path = profile_subpath(&name, "config.toml").expect("config path");
    // `api_key = "sk-c2probe-NOTAKEY` is an unterminated string: the span sits
    // at end-of-input, so the position is line 1, column 30 (29 chars, one
    // past the last).
    std::fs::write(&config_path, "api_key = \"sk-c2probe-NOTAKEY").expect("write torn config");
    let err = load_profile(&name).expect_err("a torn config fails the load");
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        "failed to parse c2-torn/config.toml at line 1, column 30"
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

#[test]
fn read_toml_file_names_the_position_and_never_the_secret() {
    let _home = HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    let roster = clauth.join("profiles.toml");
    std::fs::write(&roster, "api_key = \"sk-c2probe-NOTAKEY").expect("write torn roster");
    let err = read_toml_file::<toml::Value>(&roster).expect_err("a torn roster fails the read");
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        format!("failed to parse {} at line 1, column 30", roster.display())
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

/// A token string where the `expiresAt` number belongs is a type error; the
/// position names the spot without quoting the value.
#[test]
fn read_json_file_names_the_position_and_never_the_secret() {
    let _home = HomeSandbox::new();
    let cred = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("credentials.json");
    crate::profile::mkdir_700(cred.parent().expect("parent")).expect("mkdir .clauth");
    std::fs::write(
        &cred,
        "{\"claudeAiOauth\": {\"expiresAt\": \"sk-c2probe-NOTAKEY\"}}",
    )
    .expect("write credentials");
    let err = read_json_file::<ClaudeCredentials>(&cred)
        .expect_err("a token string where a number belongs fails the read");
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        format!("failed to parse {} at line 1, column 52", cred.display())
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

/// A json error whose column is 0 (a whole-value type mismatch) names the line
/// alone: there is no column to point at, but the file has a line to open.
#[test]
fn read_json_file_renders_line_alone_when_the_column_is_zero() {
    let _home = HomeSandbox::new();
    let cred = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("credentials.json");
    crate::profile::mkdir_700(cred.parent().expect("parent")).expect("mkdir .clauth");
    // A top-level array where a map is expected fails with line 1, column 0.
    std::fs::write(&cred, "[{\"endpoint\": \"x\"}]").expect("write credentials");
    let err = read_json_file::<std::collections::HashMap<String, u64>>(&cred)
        .expect_err("a top-level array where a map belongs fails the read");
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        format!("failed to parse {} at line 1", cred.display())
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

/// A file torn right after a newline is an end-of-input error at column 0 on
/// line 2: the empty-file carve-out is line 1 column 0 alone, so this names
/// line 2 rather than rendering the bare label.
#[test]
fn read_json_file_names_the_line_for_a_file_torn_after_a_newline() {
    let _home = HomeSandbox::new();
    let cred = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("credentials.json");
    crate::profile::mkdir_700(cred.parent().expect("parent")).expect("mkdir .clauth");
    std::fs::write(&cred, "{\"a\": 1,\n").expect("write credentials");
    let err = read_json_file::<serde_json::Value>(&cred)
        .expect_err("a file torn right after a newline fails the read");
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        format!("failed to parse {} at line 2", cred.display())
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}

/// A content writer that fails inside the open temp leaves no temp behind and
/// the target as it was.
#[test]
fn a_failing_content_writer_leaves_no_temp_and_the_target_untouched() {
    let home = HomeSandbox::new();
    let dir = home.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.json");
    std::fs::write(&path, b"old").unwrap();
    let err = atomic_write_600_with(&path, |f| {
        std::io::Write::write_all(f, b"partial")?;
        Err(std::io::Error::other("full"))
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "full");
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["f.json"], "a temp stayed behind");
}

/// The same for the plain writer behind [`atomic_write`]: a failing content
/// writer passes its error through, keeps the target and leaves no temp.
#[test]
fn a_failing_plain_content_writer_leaves_no_temp_and_the_target_untouched() {
    let home = HomeSandbox::new();
    let dir = home.home().join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    std::fs::write(&path, b"old").unwrap();
    let err = atomic_write_with(&path, |f| {
        std::io::Write::write_all(f, b"partial")?;
        Err(std::io::Error::other("full"))
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "full");
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["settings.json"], "a temp stayed behind");
}

// ── a profile's id: what a login in flight lands by ──────────────────────────

fn create_account(name: &str) {
    let mut config = load_config().expect("config");
    crate::actions::create_blank_profile(
        &mut config,
        name.to_string(),
        None,
        Some("sk-id".to_string()),
        ModelSettings::default(),
    )
    .expect("create");
}

fn claim(name: &str) -> ProfileId {
    claim_profile_id(&ProfileName::from(name)).expect("claim")
}

/// A settings edit and a whole-profile save can land while a re-login waits on
/// the browser; each keeps the id the login claimed, so the login still lands.
/// A rename carries it to the new name.
#[test]
fn a_profile_id_survives_every_config_write_and_a_rename() {
    let _home = crate::testutil::HomeSandbox::new();
    create_account("work");
    let id = claim("work");
    let name = ProfileName::from("work");

    let mut p = load_profile(&name).expect("load");
    p.auto_start = true;
    save_profile_config(&p).expect("settings edit");
    assert_eq!(claim("work"), id, "a settings edit keeps it");

    save_profile(&p).expect("whole save");
    assert_eq!(claim("work"), id, "a whole-profile save keeps it");

    let guard = crate::actions::rotation_guard_for_mutation(&name).expect("guard");
    crate::actions::rename_profile(
        &mut load_config().expect("config"),
        &name,
        &ProfileName::from("moved"),
        &guard,
    )
    .expect("rename");
    assert_eq!(claim("moved"), id, "a rename carries it");
}

/// An older clauth does not model `id`: its saves carry it below the
/// preserved-keys marker. This clauth still reads it there, and its next save
/// renders it back as its own key.
#[test]
fn a_profile_id_an_older_save_moved_is_still_the_profile_id() {
    let _home = crate::testutil::HomeSandbox::new();
    create_account("work");
    let id = claim("work");
    let name = ProfileName::from("work");
    let path = profile_config_path(&name).expect("path");
    let raw = std::fs::read_to_string(&path).expect("read");
    let id_line = raw
        .lines()
        .find(|l| l.starts_with("id = "))
        .expect("the id line")
        .to_string();
    // Where an older save leaves a carried scalar: under the marker, before the
    // first table.
    let stripped = raw.replacen(&format!("{id_line}\n\n"), "", 1);
    let carried = format!(
        "# keys preserved from the previous file; not modelled by this clauth:\n{id_line}\n"
    );
    let moved = match stripped.find("\n[") {
        Some(at) => {
            let (head, tail) = stripped.split_at(at + 1);
            format!("{head}{carried}{tail}")
        }
        None => format!("{stripped}{carried}"),
    };
    assert!(!moved.starts_with("# clauth profile configuration\n\nid = "));
    assert_eq!(moved.matches("id = ").count(), 1);
    std::fs::write(&path, moved).expect("write");
    assert_eq!(claim("work"), id, "read where the older save left it");

    save_profile_config(&load_profile(&name).expect("load")).expect("save");
    let after = std::fs::read_to_string(&path).expect("read");
    assert_eq!(
        after.matches("id = ").count(),
        1,
        "one id, not two: {after}"
    );
    assert_eq!(claim("work"), id);
}

/// A profile an older clauth created has no id until a login starts on it;
/// the claim mints one into its `config.toml` and every later claim reads it.
#[test]
fn a_claim_mints_an_id_into_a_profile_without_one() {
    let _home = crate::testutil::HomeSandbox::new();
    create_account("work");
    let name = ProfileName::from("work");
    let path = profile_config_path(&name).expect("path");
    let raw = std::fs::read_to_string(&path).expect("read");
    let id_line = raw
        .lines()
        .find(|l| l.starts_with("id = "))
        .expect("id")
        .to_string();
    std::fs::write(&path, raw.replacen(&format!("{id_line}\n"), "", 1)).expect("strip");
    let first = claim("work");
    assert!(
        std::fs::read_to_string(&path)
            .expect("read")
            .contains("id = "),
        "the claim persisted it"
    );
    assert_eq!(claim("work"), first);
}

/// A create over a directory an earlier account left behind is a new
/// account: it gets a fresh id, never the leftover's.
#[test]
fn a_create_over_a_leftover_directory_gets_a_fresh_id() {
    let _home = crate::testutil::HomeSandbox::new();
    create_account("work");
    let old = claim("work");
    save_app_state(&AppState::default()).expect("drop the record, keep the dir");
    create_account("work");
    assert_ne!(claim("work"), old);
}

/// A `~/.claude` dir clauth creates (`sessions/`, a `projects/<slug>/`) that an
/// older build left at the umask is retightened at load: never a file inside
/// it, never through a symlink, never a dir clauth does not create. Unpinned
/// here and by its siblings: the swap race itself (a link swapped in between
/// the no-follow open and the chmod, or inside the owner-read-off fallback),
/// which no seam reaches.
#[cfg(unix)]
#[test]
fn load_config_retightens_the_claude_dirs_clauth_creates() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    let chmod = |p: &std::path::Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).expect("chmod");
    };
    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;

    let sessions = claude.join("sessions");
    let slug = claude.join("projects").join("-home-u-repo");
    let foreign = claude.join("plugins");
    for dir in [&sessions, &slug, &foreign] {
        std::fs::create_dir_all(dir).expect("mkdir");
        chmod(dir, 0o755);
    }
    let record = sessions.join("123.json");
    std::fs::write(&record, b"{}").expect("write record");
    chmod(&record, 0o644);

    // A shared-mode link points into a tree clauth does not own.
    let outside = home.home().join("outside-paste-cache");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    chmod(&outside, 0o755);
    std::os::unix::fs::symlink(&outside, claude.join("paste-cache")).expect("symlink");

    load_config().expect("load_config");

    assert_eq!(mode(&sessions), 0o700, "sessions/ retightened");
    assert_eq!(mode(&slug), 0o700, "projects/<slug>/ retightened");
    assert_eq!(mode(&record), 0o644, "a file inside is left as found");
    assert_eq!(mode(&outside), 0o755, "a symlinked dir is never followed");
    assert_eq!(
        mode(&foreign),
        0o755,
        "a dir clauth never creates is left alone"
    );
}

/// The retighten never follows a `projects/<slug>` link: the link resolves
/// into the operator's tree and chmodding through it would restyle a dir
/// clauth does not own. The sibling retighten test pins a linked `paste-cache`;
/// this one pins the `projects/` child shape.
#[cfg(unix)]
#[test]
fn load_config_does_not_follow_a_symlinked_project_slug() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    std::fs::create_dir_all(claude.join("projects")).expect("mkdir projects");

    let outside = home.home().join("outside-slug");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink(&outside, claude.join("projects").join("-home-u-repo"))
        .expect("symlink");

    load_config().expect("load_config");

    let mode = std::fs::metadata(&outside)
        .expect("outside metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "a symlinked project slug is never followed");
}

/// The retighten is non-recursive: a dir one level under `sessions/` is left
/// as found. clauth's writers create `sessions/` itself, not its children —
/// those are Claude Code's, and the session-id listing is the leak this
/// closes.
#[cfg(unix)]
#[test]
fn load_config_leaves_a_dir_nested_under_sessions_as_found() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    let sessions = claude.join("sessions");
    let nested = sessions.join("some-session-id");
    std::fs::create_dir_all(&nested).expect("mkdir nested");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    load_config().expect("load_config");

    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&sessions), 0o700, "sessions/ retightened");
    assert_eq!(
        mode(&nested),
        0o755,
        "a dir one level down is left as found"
    );
}

/// A real loose `paste-cache/` is retightened too: the sibling tests pin it
/// only as a symlink, so a dropped name would leave this mode at the umask.
#[cfg(unix)]
#[test]
fn load_config_retightens_a_real_loose_paste_cache() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let paste = claude_dir().expect("claude_dir").join("paste-cache");
    std::fs::create_dir_all(&paste).expect("mkdir");
    std::fs::set_permissions(&paste, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    load_config().expect("load_config");
    let mode = std::fs::metadata(&paste)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "paste-cache/ retightened");
}

/// A regular file in a retightened slot (`projects/<slug>`) is never chmodded:
/// the set covers dirs only, so dropping the dir guard would restyle a file the
/// operator owns.
#[cfg(unix)]
#[test]
fn load_config_never_chmods_a_file_in_a_retightened_slot() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    std::fs::create_dir_all(claude.join("projects")).expect("mkdir");
    let file = claude.join("projects").join("stray.json");
    std::fs::write(&file, b"{}").expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    load_config().expect("load_config");
    let mode = std::fs::symlink_metadata(&file)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644, "a file is never chmodded");
}

/// A linked `projects/` itself lists nothing: the listing gate refuses the link
/// before iterating, so the operator's children behind it are never restyled.
#[cfg(unix)]
#[test]
fn load_config_does_not_follow_a_linked_projects() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    std::fs::create_dir_all(&claude).expect("mkdir");
    let outside = home.home().join("outside-projects");
    let child = outside.join("-home-u-repo");
    std::fs::create_dir_all(&child).expect("mkdir");
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink(&outside, claude.join("projects")).expect("symlink");
    load_config().expect("load_config");
    let mode = std::fs::metadata(&child)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "a linked projects/ lists nothing");
}

/// The retighten clears only group and other bits: a 0500 dir keeps its owner
/// r-x and a 0755 one reads 0700, so a narrowed mode is never widened back.
#[cfg(unix)]
#[test]
fn load_config_clears_only_group_and_other_bits() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    let narrow = claude.join("plans");
    let loose = claude.join("tasks");
    for dir in [&narrow, &loose] {
        std::fs::create_dir_all(dir).expect("mkdir");
    }
    std::fs::set_permissions(&narrow, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    load_config().expect("load_config");

    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&narrow), 0o500, "a 0500 dir keeps its owner bits");
    assert_eq!(mode(&loose), 0o700, "a 0755 dir loses only group and other");
}

/// A 0555 dir has group and other bits set, so the retighten runs its chmod
/// branch — unlike the sibling 0500 case, which has none and never does. The
/// branch must clear only group and other, never re-add owner write.
#[cfg(unix)]
#[test]
fn load_config_a_0555_dir_never_gains_owner_write() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let plans = claude_dir().expect("claude_dir").join("plans");
    std::fs::create_dir_all(&plans).expect("mkdir");
    std::fs::set_permissions(&plans, std::fs::Permissions::from_mode(0o555)).expect("chmod");

    load_config().expect("load_config");

    let mode = std::fs::metadata(&plans)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o500, "a 0555 dir loses group and other only");
}

/// The retighten keeps setgid as found: clearing group and other bits must not
/// strip the 02000 bit (a `mode & 0o700` mask would). The fixture check fails
/// the test if the filesystem drops setgid before `load_config` runs.
#[cfg(unix)]
#[test]
fn load_config_a_setgid_dir_keeps_setgid() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let sessions = claude_dir().expect("claude_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o2755)).expect("chmod");
    let before = std::fs::metadata(&sessions)
        .expect("meta")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(before, 0o2755, "fixture took setgid");

    load_config().expect("load_config");

    let mode = std::fs::metadata(&sessions)
        .expect("meta")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o2700, "setgid stays as found");
}

/// Every top-level name in the set — the rescue's `SIDECAR_TREES` plus
/// the store-link dirs `jobs` and `projects` — is retightened. The names are
/// hand-listed, not read from the constant, so a dropped or renamed entry reds
/// here instead of hiding in a mirrored loop.
#[cfg(unix)]
#[test]
fn load_config_retightens_every_named_claude_dir() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let claude = claude_dir().expect("claude_dir");
    let names = [
        "file-history",
        "paste-cache",
        "plans",
        "session-env",
        "sessions",
        "shell-snapshots",
        "tasks",
        "todos",
        "jobs",
        "projects",
    ];
    for name in names {
        let dir = claude.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    load_config().expect("load_config");

    for name in names {
        let mode = std::fs::metadata(claude.join(name))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{name}/ retightened");
    }
}

/// A dir whose owner read bit is off but whose group and other bits are set
/// (0377) fails the no-follow open with `EACCES`, yet those bits are exactly
/// what must go: the by-path fallback clears them to 0300 instead of skipping
/// the dir.
#[cfg(unix)]
#[test]
fn load_config_retightens_a_dir_whose_owner_read_is_off() {
    use std::os::unix::fs::PermissionsExt;

    /// Hands the dir back to 0o700 before the sandbox drops, a panic included,
    /// so the tempdir's own removal can read it.
    struct RestoreDir(std::path::PathBuf);
    impl Drop for RestoreDir {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    let _home = HomeSandbox::new();
    let sessions = claude_dir().expect("claude_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o377)).expect("chmod");
    let _restore = RestoreDir(sessions.clone());

    load_config().expect("load_config");

    let mode = std::fs::metadata(&sessions)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o300, "group and other bits cleared, owner bits kept");
}

// The usage probe defaults off at 90 s: both keys render commented until set,
// and a set switch and a non-default interval render live and parse back.
#[test]
fn the_usage_probe_defaults_off_and_round_trips_through_config_toml() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert!(!cfg.usage_probe);
    assert_eq!(cfg.usage_probe_interval_ms, None);

    let stock = Profile::new("p".to_string(), None, None);
    assert!(!stock.usage_probe, "a new profile does not probe");
    assert_eq!(stock.usage_probe_interval_ms, 90_000);
    let rendered = render_config_toml(&stock);
    assert!(rendered.contains("\n# usage_probe = true\n"), "{rendered}");
    assert!(
        rendered.contains(
            "\n# How often the usage probe runs, in milliseconds: 60000 to\n\
             # 450000. A value outside that range is clamped into it.\n\
             # usage_probe_interval_ms = 90000\n"
        ),
        "{rendered}"
    );
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse stock toml");
    assert!(!parsed.usage_probe);
    assert_eq!(parsed.usage_probe_interval_ms, None);

    let mut probed = Profile::new("p".to_string(), None, None);
    probed.usage_probe = true;
    probed.usage_probe_interval_ms = 120_000;
    let rendered = render_config_toml(&probed);
    assert!(rendered.contains("\nusage_probe = true\n"), "{rendered}");
    assert!(
        rendered.contains("\nusage_probe_interval_ms = 120000\n"),
        "{rendered}"
    );
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert!(parsed.usage_probe);
    assert_eq!(parsed.usage_probe_interval_ms, Some(120_000));
}

// A hand-edited probe interval loads inside 1 min ..= 7.5 min, the edges
// included, and an absent one loads as the default.
#[test]
fn a_stored_probe_interval_loads_inside_its_band() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("probe-interval");
    save_profile(&crate::testutil::blank_profile(&name)).expect("save_profile");
    let config_path = profile_subpath(&name, "config.toml").expect("config path");
    for (stored, loaded) in [
        (0, 60_000),
        (59_999, 60_000),
        (60_000, 60_000),
        (90_000, 90_000),
        (450_000, 450_000),
        (450_001, 450_000),
        (600_000, 450_000),
        (3_600_000, 450_000),
    ] {
        std::fs::write(
            &config_path,
            format!("usage_probe = true\nusage_probe_interval_ms = {stored}\n"),
        )
        .expect("write config.toml");
        let profile = load_profile(&name).expect("load");
        assert!(profile.usage_probe);
        assert_eq!(profile.usage_probe_interval_ms, loaded, "stored {stored}");
    }
    std::fs::write(&config_path, "usage_probe = true\n").expect("write config.toml");
    assert_eq!(
        load_profile(&name).expect("load").usage_probe_interval_ms,
        90_000,
        "absent loads as the default"
    );
}

/// The loader is where a profile gets its setup token's digest, the key a
/// refused token's durable verdict is bound to: a pairless profile loaded
/// from disk whose token the probe refused is passed over by the walks and
/// `clauth start --auto`, through the verdict the loaded digest matches.
#[test]
fn a_refused_setup_token_on_a_profile_loaded_from_disk_is_passed_over() {
    let _sb = HomeSandbox::new();
    let name = ProfileName::from("refused");
    let names = vec![name.clone(), ProfileName::from("next")];
    save_app_state(&AppState {
        active_profile: Some(name.clone()),
        profiles: names.clone(),
        fallback_chain: names,
        ..AppState::default()
    })
    .expect("persist the chain");
    let mut probed = crate::testutil::blank_profile(&name);
    crate::testutil::make_probed_account(&mut probed);
    let mut next = crate::testutil::blank_profile(&ProfileName::from("next"));
    next.credentials = Some(crate::testutil::oauth_pair("next"));
    save_profile(&next).expect("save next");
    let digest = crate::claude::installed_session_token(&name)
        .and_then(|token| crate::usage::OAuthIdentity::for_setup_token(&token).setup_token)
        .expect("the installed token's digest");
    crate::profile_cache::write_auth_expired(&name, digest);

    let config = load_config().expect("load_config");
    let (rows, picked) = crate::fallback::start_walk(
        &config,
        crate::runtime::Isolation::Shared,
        &[],
        [None, None],
        false,
        None,
    );
    assert_eq!(picked, Some(1), "the walk passes the refused token over");
    assert_eq!(rows[0].block, Some(crate::fallback::StartBlock::AuthBroken));
    let loaded = config.find(&name).expect("the loaded profile");
    assert_eq!(loaded.setup_token_digest, Some(digest));
    assert!(
        crate::fallback::durable_key_rejected_seed([loaded]).contains_key("refused"),
        "the verdict binds to the digest the loader read"
    );
}
