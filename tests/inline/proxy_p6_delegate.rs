#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A delegate on a stub-proxy profile passes the spawn gate. The
//! gate's refusals key on inference auth, which a proxy profile carries (its
//! proxy-local inference key); this pins that the gate stays true to that
//! once the profile stops reading as a generic endpoint. Mounted inside
//! `mcp` to reach `preflight_target`, the guard itself.

use super::*;

/// The spawn gate under the demand a delegate naming no model carries: the
/// families and credit-billed set `run_delegate` reads, derived by the same
/// calls (shared isolation, the delegate default).
fn gate(profile: &Profile, config: &AppConfig, name: &ProfileName) -> Result<(), String> {
    let stale_env_keys = crate::actions::outgoing_env_keys(config);
    let (models, families, agent_without_model, unknown) = delegate_families(
        profile,
        Isolation::Shared,
        None,
        None,
        &[],
        &HashMap::new(),
        &stale_env_keys,
        None,
        None,
    );
    let credit_billed = crate::claude_json::credit_billed_families();
    preflight_target(
        profile,
        config,
        name,
        &models,
        &families,
        agent_without_model.as_deref(),
        unknown,
        &credit_billed,
    )
}

/// The stub-proxy profile with its key: admitted, where a disabled profile is
/// refused by the same gate on the same call.
#[test]
fn a_delegate_on_a_stub_proxy_profile_passes_the_spawn_gate() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    let profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_stub-key-0".to_string()),
    );
    config.add(profile);

    let name = crate::profile::ProfileName::from("px");
    let profile = config.find(&name).unwrap();
    gate(profile, &config, &name)
        .expect("the spawn gate admits a proxy profile holding its inference key");

    // The control on the same gate: a disabled profile is refused, so the
    // admission above is the gate answering, not the call being inert.
    let mut disabled = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_stub-key-0".to_string()),
    );
    disabled.disabled = true;
    config.profiles.clear();
    config.add(disabled);
    let profile = config.find(&name).unwrap();
    gate(profile, &config, &name).expect_err("the same gate refuses a disabled profile");
}

/// A proxy account whose own last fetched verdict says it cannot fund a call
/// (`is_available: false` in the third-party cache its usage leg rewrites every
/// poll) is refused BEFORE spawning, like any provider account; the same
/// profile with a funded verdict is admitted, so the refusal is the verdict
/// answering, not the profile.
#[test]
fn a_proxy_account_the_proxy_reports_unfunded_is_refused_before_spawning() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    let profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_stub-key-0".to_string()),
    );
    crate::profile::save_profile(&profile).expect("save profile");
    crate::profile::write_proxy_record(
        &profile.name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    // Loaded, the shape every caller of the gate holds: the record rides the
    // profile from its load.
    config.add(crate::profile::load_profile(&profile.name).expect("load profile"));
    let name = crate::profile::ProfileName::from("px");

    // One body row and no bars: the headline is that row, `label: value`,
    // with `(unavailable)` beside it when the verdict is unfunded and no
    // verdict row names a reason (`render::third_party_headline`).
    let verdict = |available: bool| crate::providers::ThirdPartyStats {
        is_available: available,
        rows: vec![crate::providers::StatRow {
            label: crate::providers::DEEPSEEK_BALANCE_ROW_LABEL.to_string(),
            value: "0.00 CNY".to_string(),
            kind: crate::providers::StatRowKind::Body,
            raw: None,
        }],
        bars: vec![],
        plan: None,
        endpoint: None,
        best_effort: false,
    };

    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &verdict(false),
    );
    let refusal = gate(config.find(&name).unwrap(), &config, &name)
        .expect_err("an unfunded proxy account is refused before spawning");
    assert_eq!(
        refusal,
        "cannot fund a run: px — api balance: 0.00 CNY (unavailable) (cached just now); \
         name another account"
    );

    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &verdict(true),
    );
    gate(config.find(&name).unwrap(), &config, &name)
        .expect("the same proxy account with a funded verdict is admitted");
}

/// A KEYLESS proxy account is outside the cannot-pay arm: no usage leg polls
/// it (the leg's own credential test), so a cached unfunded verdict on it is
/// one nothing would refresh, and the arm leaves it inert as it does any
/// other profile outside its bound. The keyless arm refuses it instead,
/// naming the login through its proxy, never the unfunded verdict.
#[test]
fn a_keyless_proxy_account_is_outside_the_cannot_pay_arm() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    let profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
    );
    crate::profile::save_profile(&profile).expect("save profile");
    crate::profile::write_proxy_record(
        &profile.name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    // Loaded, so the record rides the profile and the keyless test is what
    // keeps it out of the arm.
    config.add(crate::profile::load_profile(&profile.name).expect("load profile"));
    let name = crate::profile::ProfileName::from("px");
    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: false,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    assert_eq!(
        crate::usage::collect_third_party_entries(&config.profiles).len(),
        0,
        "no leg polls it"
    );

    assert_eq!(
        gate(config.find(&name).unwrap(), &config, &name),
        Err("profile has no api key: px (run `clauth login px --proxy stub`)".to_string())
    );
}

/// The `profiles` roster's `stale` flag, a routing flag, judges a proxy
/// account by its chain windows' date as the start walk does: a copy whose
/// chain window was read a minute ago carries no `stale` beside a 3 h-old
/// stalest figure, and the same copy with its chain window 3 h old does. A
/// copy with no chain date keeps the age rule: 3 h old reads stale.
#[test]
fn the_roster_flags_a_proxy_account_stale_by_its_chain_windows() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_stub-key-0".to_string()),
    );
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    crate::profile::save_profile(&profile).expect("save profile");
    config.add(profile);
    crate::profile::save_app_state(&config.state).expect("save state");
    let name = crate::profile::ProfileName::from("px");
    let stats = crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]);
    let now_s = crate::usage::now_epoch_secs();
    let at = |secs: i64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
    let ms = |secs: i64| (secs as u64) * 1000;
    let stale = |chain_read_at: Option<u64>| {
        assert!(crate::profile_cache::write_third_party_copy(
            &name,
            &stats,
            chain_read_at,
            ms(now_s),
            at(now_s - 3 * 3600),
        ));
        let live = crate::live_sessions::LiveTally::collect(&config);
        let row = profile_row(config.find(&name).unwrap(), &config, now_s, &live);
        row["windows"]["stale"].as_bool()
    };

    assert_eq!(stale(Some(ms(now_s - 60))), None, "a fresh chain window");
    assert_eq!(stale(Some(ms(now_s - 3 * 3600))), Some(true), "a stale one");
    assert_eq!(stale(None), Some(true), "no chain date: the age decides");
}

/// The cannot-pay arm asks a LOADED profile whether it is a proxy account off
/// the record it loaded with, parsing no `config.toml` per preflight; the
/// refusal is the control that the arm read it as one.
#[test]
fn the_preflight_reads_a_loaded_proxy_accounts_record_off_the_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let name = crate::profile::ProfileName::from("px");
    crate::profile::save_profile(&crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_stub-key-0".to_string()),
    ))
    .expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    config.add(crate::profile::load_profile(&name).expect("load profile"));
    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: false,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let stale_env_keys = crate::actions::outgoing_env_keys(&config);
    let profile = config.find(&name).unwrap();
    let (models, families, agent_without_model, unknown) = delegate_families(
        profile,
        Isolation::Shared,
        None,
        None,
        &[],
        &HashMap::new(),
        &stale_env_keys,
        None,
        None,
    );
    let credit_billed = crate::claude_json::credit_billed_families();
    let before = crate::profile::proxy_claim_reads();

    let refused = preflight_target(
        profile,
        &config,
        &name,
        &models,
        &families,
        agent_without_model.as_deref(),
        unknown,
        &credit_billed,
    )
    .is_err();

    assert_eq!(
        (refused, crate::profile::proxy_claim_reads() - before),
        (true, 0),
        "(refused, record reads)"
    );
}

/// A proxy account the keyless arm admits (an `[env]` token, no managed key)
/// that no usage leg polls keeps a stranded unfunded verdict inert: the
/// cannot-pay arm's credential test is what keeps it out of the arm.
#[test]
fn an_env_token_proxy_account_is_outside_the_cannot_pay_arm() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    let mut profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
    );
    profile
        .env
        .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "tok-env".to_string());
    crate::profile::save_profile(&profile).expect("save profile");
    crate::profile::write_proxy_record(
        &profile.name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    config.add(crate::profile::load_profile(&profile.name).expect("load profile"));
    let name = crate::profile::ProfileName::from("px");
    let loaded = config.find(&name).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.is_keyless(),
            crate::usage::third_party_credentialed(loaded),
        ),
        (true, false, false),
        "fixture control: (the runtime proxy claim, keyless, credentialed)"
    );
    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: false,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    assert_eq!(gate(config.find(&name).unwrap(), &config, &name), Ok(()));
}
