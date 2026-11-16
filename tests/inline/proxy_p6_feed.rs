#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `list` and `status --json` keep working on a proxy profile. Both
//! surfaces render over `build_profile_entries` (the one data path), so the
//! pin drives that builder over a proxy profile on disk and the full
//! `build_status` JSON: the profile lists with its base_url, nothing crashes,
//! and no credential byte is in the output. Mounted inside `daemon` to reach
//! the private `status_json` module.

use super::status_json::build_status;

/// A stub-proxy profile renders in the feed like any endpoint profile: its
/// base_url, its name, and none of its key.
#[test]
fn list_and_status_json_keep_working_on_a_proxy_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let path = crate::proxy::registry_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "[stub]\nport = 9141\nenabled = true\n").unwrap();

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

    let body = build_status(&config, 90_000, None, false);
    let row = body
        .profiles
        .iter()
        .find(|entry| entry.name.as_str() == "px")
        .expect("the proxy profile lists");
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:9141"));

    let json = serde_json::to_string(&body).expect("the body serializes");
    assert!(json.contains("http://127.0.0.1:9141"), "the base_url rides");
    assert!(
        !json.contains("clp_stub-key-0"),
        "no credential byte reaches the output: {json}"
    );
}

thread_local! {
    static RUNG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn record_ring(_template: &str, message: &str) -> anyhow::Result<()> {
    RUNG.with(|r| r.borrow_mut().push(message.to_string()));
    Ok(())
}

/// What the daemon's alerts did on one `write_status` over `config`, the
/// scheduler's third-party leg statuses `statuses` as its live store: the 5h
/// bell lines it rang, the pushes it queued, and the `stale` flag
/// `status.json` published for `name`. Drives the production gates
/// (`ring_bells`, `queue_pushes`); the bell runs through a recorder.
pub(crate) fn alerts_on_one_tick(
    config: crate::profile::AppConfig,
    statuses: std::collections::HashMap<String, crate::usage::FetchStatus>,
    name: &str,
) -> (Vec<String>, Vec<crate::push::Trigger>, bool) {
    RUNG.with(|r| r.borrow_mut().clear());
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    let mut daemon = super::Daemon::new(config, status_path.clone());
    daemon.ring_bell = record_ring;
    let (queue, inbox) = crate::push::queue_for_tests();
    daemon.push = Some(queue);
    *daemon.third_party_status.lock().expect("statuses") = statuses;

    daemon.write_status();

    let body: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&status_path).expect("status.json")).expect("json");
    let published_stale = body["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == name)
        .and_then(|entry| entry["stale"].as_bool())
        .expect("the entry's stale");
    (
        RUNG.with(|r| r.borrow().clone()),
        inbox.try_iter().collect(),
        published_stale,
    )
}

/// The single-shot feed (`clauth status --json`, no daemon) derives a proxy
/// copy's `fetch_status` off its chain windows' date and its `next_refresh_at`
/// off the write instant it records, the clocks the startup seed reads,
/// never off the mtime it dates back to its stalest figure. `stale` and
/// `fetched_at` keep that mtime. A copy without the fields keeps
/// the mtime for all four.
#[test]
fn the_single_shot_feed_dates_a_proxy_copys_poll_off_its_recorded_clocks() {
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
    let parsed = |iso: &str| {
        chrono::DateTime::parse_from_rfc3339(iso)
            .expect("an rfc3339 stamp")
            .timestamp()
    };
    let row = |config: &crate::profile::AppConfig| {
        let body = build_status(config, 90_000, None, false);
        let row = body
            .profiles
            .into_iter()
            .find(|entry| entry.name.as_str() == "px")
            .expect("the proxy profile lists");
        (
            row.fetch_status.clone(),
            row.next_refresh_at.as_deref().map(parsed),
            row.stale,
            row.fetched_at.as_deref().map(parsed),
        )
    };

    // Chain windows read 60 s ago, written 30 s ago, stalest figure 3 h old.
    assert!(crate::profile_cache::write_third_party_copy(
        &name,
        &stats,
        Some(ms(now_s - 60)),
        ms(now_s - 30),
        at(now_s - 3 * 3600),
    ));
    assert_eq!(
        row(&config),
        (
            Some("Fresh".to_string()),
            Some(now_s - 30 + 90),
            true,
            Some(now_s - 3 * 3600)
        ),
        "a copy carrying the fields"
    );

    // Chain windows read 3 h ago and written 30 s ago: the status is the
    // chain's, the next refresh the write's.
    assert!(crate::profile_cache::write_third_party_copy(
        &name,
        &stats,
        Some(ms(now_s - 3 * 3600)),
        ms(now_s - 30),
        at(now_s - 3 * 3600),
    ));
    assert_eq!(
        row(&config),
        (
            Some("Cached".to_string()),
            Some(now_s - 30 + 90),
            true,
            Some(now_s - 3 * 3600)
        ),
        "a copy whose chain windows are older than its write"
    );

    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    crate::testutil::set_mtime(
        &crate::profile::profile_dir(&name)
            .unwrap()
            .join(crate::profile_cache::THIRD_PARTY_CACHE_FILE),
        at(now_s - 3 * 3600),
    );
    assert_eq!(
        row(&config),
        (
            Some("Cached".to_string()),
            None,
            true,
            Some(now_s - 3 * 3600)
        ),
        "a copy without them keeps the mtime"
    );
}

/// A proxy profile whose key is gone (cleared on the setup tab, or a
/// hand-edited config) is still a third-party usage account by the `[proxy]`
/// record it loaded with. It is never polled, and its feed entry still
/// publishes the copy's last figures with the copy's clocks, never figures
/// with no age beside an absent OAuth cache's empty status. Its
/// `next_refresh_at` is `null` from the moment its key is gone: no refresh
/// that never comes is promised.
#[test]
fn a_keyless_proxy_profile_publishes_its_copy_with_its_clocks() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("px");
    crate::profile::save_profile(&crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
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
    crate::profile::save_app_state(&config.state).expect("save state");
    let now_s = crate::usage::now_epoch_secs();
    let ms = |secs: i64| (secs as u64) * 1000;
    // Chain windows read 60 s ago, written 30 s ago, stalest figure 3 h old.
    assert!(crate::profile_cache::write_third_party_copy(
        &name,
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]),
        Some(ms(now_s - 60)),
        ms(now_s - 30),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs((now_s - 3 * 3600) as u64),
    ));

    let body = build_status(&config, 90_000, None, false);
    let row = body
        .profiles
        .into_iter()
        .find(|entry| entry.name.as_str() == "px")
        .expect("the proxy profile lists");
    let parsed = |iso: &str| {
        chrono::DateTime::parse_from_rfc3339(iso)
            .expect("an rfc3339 stamp")
            .timestamp()
    };
    assert_eq!(
        (
            row.fetch_status.clone(),
            row.fetched_at.as_deref().map(parsed),
            row.next_refresh_at.as_deref().map(parsed),
        ),
        (Some("Fresh".to_string()), Some(now_s - 3 * 3600), None),
        "the copy's clocks date its figures, and no refresh is promised"
    );
    assert!(!row.windows.is_empty(), "the copy's figures publish");
}

/// A loaded profile answers which usage cache it reads without parsing its
/// config.toml again: the render, seed and status paths ask
/// per frame, per tick and per write. The `[proxy]` claim is read once, when
/// the profile loads, so a keyless proxy account still reads its copy, and a
/// keyless local endpoint with no record (an LM Studio shape) reads none.
#[test]
fn a_loaded_profile_answers_its_cache_kind_without_reading_its_config() {
    let _home = crate::testutil::HomeSandbox::new();
    for name in ["px", "lms"] {
        crate::profile::save_profile(&crate::profile::Profile::new(
            name.to_string(),
            Some("http://127.0.0.1:9141".to_string()),
            None,
        ))
        .expect("save profile");
    }
    crate::testutil::register_names(&["px", "lms"]);
    crate::profile::write_proxy_record(
        &crate::profile::ProfileName::from("px"),
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    let load = |name: &str| {
        crate::profile::load_profile(&crate::profile::ProfileName::from(name))
            .expect("load profile")
    };
    let (px, lms) = (load("px"), load("lms"));

    let before = crate::profile::proxy_claim_reads();
    let answers = (
        px.usage_cache_is_third_party(),
        px.usage_cache_is_third_party(),
        lms.usage_cache_is_third_party(),
        lms.usage_cache_is_third_party(),
    );
    assert_eq!(
        (answers, crate::profile::proxy_claim_reads() - before),
        ((true, true, false, false), 0)
    );
}

/// A status write over a loaded keyless proxy account reads no proxy record:
/// its cache, its clocks and its published windows are all classified off the
/// profile in hand, so a per-tick write parses no `config.toml` for one. The
/// windows publishing is the control that the copy was read as the account's.
#[test]
fn a_status_write_reads_no_proxy_record() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("px");
    crate::profile::save_profile(&crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
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
    crate::profile::save_app_state(&config.state).expect("save state");
    let now_s = crate::usage::now_epoch_secs();
    assert!(crate::profile_cache::write_third_party_copy(
        &name,
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]),
        Some((now_s as u64 - 60) * 1000),
        (now_s as u64 - 30) * 1000,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(now_s as u64 - 60),
    ));
    let before = crate::profile::proxy_claim_reads();

    let row = build_status(&config, 90_000, None, false)
        .profiles
        .into_iter()
        .find(|entry| entry.name.as_str() == "px")
        .expect("the proxy profile lists");

    assert_eq!(
        (
            row.windows.len(),
            crate::profile::proxy_claim_reads() - before
        ),
        (1, 0),
        "(published windows, record reads)"
    );
}

/// The proxy profile `px` (armed at 90% when `bell` is set, a fallback chain
/// member) saved in a sandbox, its copy holding a 5h window at `util` whose
/// chain date is 1 h ahead of the clock (the clock stepped back after the
/// write), its stalest figure 60 s old.
fn chain_ahead_of_the_clock(util: f64, bell: bool) -> crate::profile::AppConfig {
    let mut profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_stub-key-0".to_string()),
    );
    if bell {
        profile.bell_threshold = Some(90.0);
    }
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    if bell {
        config.state.bell_command = Some("notify-send %s".to_string());
    }
    config.state.fallback_chain = vec!["px".into()];
    crate::profile::save_profile(&profile).expect("save profile");
    config.add(profile);
    crate::profile::save_app_state(&config.state).expect("save state");
    let now_s = crate::usage::now_epoch_secs();
    let ahead_ms = ((now_s + 3600) as u64) * 1000;
    assert!(crate::profile_cache::write_third_party_copy(
        &crate::profile::ProfileName::from("px"),
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", util)]),
        Some(ahead_ms),
        ahead_ms,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs((now_s - 60) as u64),
    ));
    config
}

/// A chain date ahead of the clock is undated, so the single-shot feed never
/// derives `Fresh` from it, nor falls back to the 60 s old mtime that would
/// earn one: it publishes `Cached`, and `stale` keeps reading the mtime.
#[test]
fn the_single_shot_feed_never_derives_fresh_from_a_chain_date_ahead_of_the_clock() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = chain_ahead_of_the_clock(10.0, false);

    let row = build_status(&config, 90_000, None, false)
        .profiles
        .into_iter()
        .find(|entry| entry.name.as_str() == "px")
        .expect("the proxy profile lists");

    assert_eq!(
        (row.fetch_status.clone(), row.stale),
        (Some("Cached".to_string()), false),
        "(fetch_status, stale) of a copy whose chain date is 1 h ahead"
    );
}

/// The daemon's 5h bell and window push read a chain date ahead of the clock
/// as undated, so stale: the chain window at 97% under a `Fresh` status rings
/// neither, though the 60 s old mtime leaves the published `stale` false.
#[test]
fn the_alerts_never_ring_on_a_chain_date_ahead_of_the_clock() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = chain_ahead_of_the_clock(97.0, true);
    let statuses =
        std::collections::HashMap::from([("px".to_string(), crate::usage::FetchStatus::Fresh)]);

    assert_eq!(
        alerts_on_one_tick(config, statuses, "px"),
        (vec![], vec![], false),
        "(bell lines, pushes, published stale) of a copy whose chain date is 1 h ahead"
    );
}

/// The live feed (a daemon's status write, the API's status) promises no
/// refresh for a keyless proxy profile either, whatever countdown its store
/// holds from before the key went; a keyed proxy profile and a keyless
/// provider profile with no `[proxy]` record keep their countdowns verbatim.
#[test]
fn the_live_feed_promises_no_refresh_for_a_keyless_proxy_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let record = crate::profile::ProxyAccount {
        service: crate::proxy::Service::parse("stub").unwrap(),
        account: "acct-1".to_string(),
    };
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    for (name, base_url, key, proxy) in [
        ("px", "http://127.0.0.1:9141", None, true),
        ("pk", "http://127.0.0.1:9141", Some("clp_stub-key-0"), true),
        ("ds", "https://api.deepseek.com/anthropic", None, false),
    ] {
        let pn = crate::profile::ProfileName::from(name);
        crate::profile::save_profile(&crate::profile::Profile::new(
            name.to_string(),
            Some(base_url.to_string()),
            key.map(str::to_string),
        ))
        .expect("save profile");
        if proxy {
            crate::profile::write_proxy_record(&pn, &record).expect("write the proxy record");
        }
        config.add(crate::profile::load_profile(&pn).expect("load profile"));
    }
    crate::profile::save_app_state(&config.state).expect("save state");
    let now_s = crate::usage::now_epoch_secs();
    let countdown = ((now_s + 45) as u64) * 1000;
    let next_refresh: std::collections::HashMap<_, _> = ["px", "pk", "ds"]
        .into_iter()
        .map(|name| {
            (
                crate::usage::LegKey {
                    leg: crate::usage::FetchLeg::ThirdParty,
                    name: crate::profile::ProfileName::from(name),
                },
                countdown,
            )
        })
        .collect();
    let empty = std::collections::HashMap::new();
    let live = super::status_json::LiveSignals {
        fetches_here: true,
        status: &empty,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &next_refresh,
        streaks: &Default::default(),
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        usage: &Default::default(),
        gateway: None,
        proxies: None,
        listen: None,
    };

    let body = build_status(&config, 90_000, Some(&live), false);
    let next = |name: &str| {
        body.profiles
            .iter()
            .find(|entry| entry.name.as_str() == name)
            .expect("the profile lists")
            .next_refresh_at
            .as_deref()
            .map(|iso| {
                chrono::DateTime::parse_from_rfc3339(iso)
                    .expect("an rfc3339 stamp")
                    .timestamp()
            })
    };
    assert_eq!(
        (next("px"), next("pk"), next("ds")),
        (None, Some(now_s + 45), Some(now_s + 45)),
        "(keyless proxy, keyed proxy, keyless provider) next refresh"
    );
}

/// A profile carrying a `[proxy]` record that holds an OAuth login and no
/// endpoint is polled by no leg: clauth never uses a proxy account's stored
/// OAuth chain, its `/usage` poll included, so its feed entry promises no
/// refresh, though a stale OAuth-leg countdown sits in the live signals. The
/// shape is built on disk, as a hand edit or any path writing an OAuth login
/// under a kept record leaves it.
#[test]
fn the_feed_promises_no_oauth_refresh_for_a_proxy_profile_holding_an_oauth_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("px");
    let mut profile = crate::profile::Profile::new("px".to_string(), None, None);
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "r16-access".to_string(),
            refresh_token: Some("r16-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    crate::testutil::register_names(&["px"]);
    let config = crate::profile::load_config().expect("load config");
    let polled_by_oauth = crate::usage::collect_token_seeds(&config)
        .iter()
        .any(|seed| seed.name.as_str() == "px");
    let now_s = crate::usage::now_epoch_secs();
    let next_refresh: std::collections::HashMap<_, _> = [(
        crate::usage::LegKey {
            leg: crate::usage::FetchLeg::OAuth,
            name: name.clone(),
        },
        ((now_s + 45) as u64) * 1000,
    )]
    .into_iter()
    .collect();
    let empty = std::collections::HashMap::new();
    let live = super::status_json::LiveSignals {
        fetches_here: true,
        status: &empty,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &next_refresh,
        streaks: &Default::default(),
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        usage: &Default::default(),
        gateway: None,
        proxies: None,
        listen: None,
    };
    let body = build_status(&config, 90_000, Some(&live), false);
    let next = body
        .profiles
        .iter()
        .find(|entry| entry.name.as_str() == "px")
        .expect("px lists")
        .next_refresh_at
        .as_deref()
        .map(|iso| {
            chrono::DateTime::parse_from_rfc3339(iso)
                .expect("an rfc3339 stamp")
                .timestamp()
        });
    assert_eq!(
        (polled_by_oauth, next),
        (false, None),
        "(the OAuth leg polls it, next_refresh_at)"
    );
}

/// A profile whose `[proxy]` table clauth cannot read, holding an OAuth login
/// and no key, is polled by no leg either, so its feed entry promises no
/// refresh, though a stale OAuth-leg countdown sits in the live signals.
#[test]
fn the_feed_promises_no_refresh_for_a_profile_with_an_unreadable_proxy_record() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("px");
    let mut profile = crate::profile::Profile::new("px".to_string(), None, None);
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "unreadable-access".to_string(),
            refresh_token: Some("unreadable-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    let path = crate::profile::profile_subpath(&name, "config.toml").expect("config path");
    let text = std::fs::read_to_string(&path).expect("read config.toml");
    let edited = text.replace("account = \"acct-1\"", "account = \"\"");
    assert_ne!(
        edited, text,
        "fixture control: the record's account was emptied"
    );
    std::fs::write(&path, edited).expect("write config.toml");
    crate::testutil::register_names(&["px"]);
    let config = crate::profile::load_config().expect("load config");
    assert!(
        matches!(
            crate::profile::stored_proxy_claim(&name),
            crate::profile::ProxyClaim::Unreadable
        ),
        "fixture control: the proxy claim is unreadable"
    );
    let now_s = crate::usage::now_epoch_secs();
    let next_refresh: std::collections::HashMap<_, _> = [(
        crate::usage::LegKey {
            leg: crate::usage::FetchLeg::OAuth,
            name: name.clone(),
        },
        ((now_s + 45) as u64) * 1000,
    )]
    .into_iter()
    .collect();
    let empty = std::collections::HashMap::new();
    let live = super::status_json::LiveSignals {
        fetches_here: true,
        status: &empty,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &next_refresh,
        streaks: &Default::default(),
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        usage: &Default::default(),
        gateway: None,
        proxies: None,
        listen: None,
    };
    let body = build_status(&config, 90_000, Some(&live), false);
    let next = body
        .profiles
        .iter()
        .find(|entry| entry.name.as_str() == "px")
        .expect("px lists")
        .next_refresh_at
        .clone();
    assert_eq!(next, None, "next_refresh_at");
}
