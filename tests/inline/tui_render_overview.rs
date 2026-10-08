//! `fallback_flow_lines`'s all-exhausted "resumes: <name> in ~<eta>" caption
//! (issue #10 follow-up) — the sibling of the "switching to <name> in ~<eta>"
//! projection line, driven by `crate::fallback::soonest_resume`.
//! Plus the overview-row state cues: marker precedence + countdown fetch cue.

use super::*;
use ratatui::style::Modifier;

use crate::fallback::BlockedReason;
use crate::profile::{AppState, ClaudeCredentials, OAuthToken, ProfileName};
use crate::usage::{FetchLeg, FetchStatus, UsageInfo, epoch_secs_to_iso, now_epoch_secs};
use std::collections::BTreeMap;

/// Build a `ReadingsSnapshot` seed the render helpers read, keyed by name.
fn snap_with_usage(mut s: ReadingsSnapshot, name: &str, u: UsageInfo) -> ReadingsSnapshot {
    s.usage.insert(name.to_string(), u);
    s
}
fn snap_with_stats(
    mut s: ReadingsSnapshot,
    name: &str,
    st: crate::providers::ThirdPartyStats,
) -> ReadingsSnapshot {
    s.third_party.insert(name.to_string(), st);
    s
}

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

/// A slow burner whose reset lands well before it would top out reads as a
/// low-drain suffix: the plain `util_color` of the rate (dim), never the
/// runs-dry WARNING escalation.
#[test]
fn drain_reset_style_low_drain_is_dim_hue() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(3_600)), // resets in 1h
    };
    // 1 %/h from 50% → ~50h to 100%, far past the 1h reset → not runs-dry.
    let style = drain_reset_style(Some(1.0), "h", &w).expect("a positive rate yields a style");
    assert_eq!(
        style.fg,
        Some(theme::util_color(1.0)),
        "a slow drain colors by util_color (dim), not warning",
    );
    assert_ne!(
        style.fg,
        theme::warning().fg,
        "a slow drain must not read as the runs-dry warning",
    );
}

/// A fast burner that will hit 100% before its reset flips the suffix to the
/// flat WARNING tint regardless of the rate's own `util_color` band.
#[test]
fn drain_reset_style_runs_dry_first_is_warning() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(360_000)), // resets in 100h
    };
    // 50 %/h from 50% → ~1h to 100%, well before the 100h reset → runs dry.
    let style = drain_reset_style(Some(50.0), "h", &w).expect("a positive rate yields a style");
    assert_eq!(
        style.fg,
        theme::warning().fg,
        "running dry before the reset escalates to warning",
    );
    // Proves the escalation overrides the rate's own band (util_color(50) = dim).
    assert_ne!(style.fg, Some(theme::util_color(50.0)));
}

/// No positive burn rate (too little history, or a window too young for an avg
/// pace) yields no style, so the caller keeps the faint default.
#[test]
fn drain_reset_style_none_without_a_positive_rate() {
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(3_600)),
    };
    assert!(
        drain_reset_style(None, "h", &w).is_none(),
        "no rate → no style"
    );
    assert!(
        drain_reset_style(Some(0.0), "h", &w).is_none(),
        "a flat rate → no style",
    );
}

/// A 7d rate arrives in %/d, so the runs-dry projection must divide by 24
/// rather than reading %/d as %/h — which would over-project the drain by 24x
/// and paint an idle weekly window amber.
#[test]
fn drain_reset_style_reads_a_7d_rate_as_per_day() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(2 * 86_400)), // resets in 2d
    };
    // 10 %/d from 50% → 5d to 100%, past the 2d reset → not runs-dry.
    let style = drain_reset_style(Some(10.0), "d", &w).expect("a positive rate yields a style");
    assert_eq!(
        style.fg,
        Some(theme::util_color(10.0)),
        "a weekly window that outlasts its drain hues by util_color, not warning",
    );
    // The same figure misread as %/h → ~5h to 100% → runs dry → warning.
    assert_eq!(
        drain_reset_style(Some(10.0), "h", &w).map(|s| s.fg),
        Some(theme::warning().fg),
        "guards the unit: %/h from the same number does escalate",
    );
}

/// 40 %/d from 50% → ~1.25d to 100%, before the 2d reset → runs dry.
#[test]
fn drain_reset_style_7d_runs_dry_first_is_warning() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(2 * 86_400)),
    };
    let style = drain_reset_style(Some(40.0), "d", &w).expect("a positive rate yields a style");
    assert_eq!(style.fg, theme::warning().fg);
}

/// `drain_rate` must source a rate for BOTH windows of an api-key/provider
/// profile. These have no `UsageInfo` and no burn history at all, so the
/// `active_burn_rate` path yields nothing — the window's own average pace is
/// the only source, and it needs only the window's utilization + `resets_at`.
#[test]
fn drain_rate_covers_third_party_windows_from_avg_pace() {
    let _home = crate::testutil::HomeSandbox::new();
    let (p, p_readings) = third_party_profile(60.0, 30.0);
    let (config, readings) = config_with(vec![(p, p_readings)], None, vec![]);
    let app = App::new(config);
    let profile = &app.config().profiles[0];
    let (five, seven) = overview_windows(profile, &readings);
    let five = five.expect("5h bar synthesizes a window");
    let seven = seven.expect("7d bar synthesizes a window").window;

    assert!(
        app.active_burn_rate(
            &crate::profile::ProfileName::from("tp"),
            &UsageInfo::default()
        )
        .is_none(),
        "no burn history exists for an api-key profile — avg pace is the only source",
    );
    let five_rate = drain_rate(
        &app,
        &crate::profile::ProfileName::from("tp"),
        profile,
        LABEL_5H,
        &five,
        &readings,
    )
    .expect("a half-elapsed 5h window yields an avg pace");
    let seven_rate = drain_rate(
        &app,
        &crate::profile::ProfileName::from("tp"),
        profile,
        LABEL_7D,
        &seven,
        &readings,
    )
    .expect("a half-elapsed 7d window yields an avg pace");
    assert!(five_rate > 0.0 && seven_rate > 0.0);

    // 60% over the 2.5h elapsed half of a 5h window is past its 50% ideal line,
    // so the cap applies: 70 / 3h = 23.3 %/h rather than the plain 24.
    assert!(
        (five_rate - 23.333).abs() < 0.1,
        "5h rate in %/h: {five_rate}"
    );
    // 30% at the 3.5d half of a 7d window is under the line, so it reads the
    // plain 30 / 3.5d ≈ 8.57 %/d untouched.
    assert!(
        (seven_rate - 30.0 / 3.5).abs() < 0.2,
        "7d rate in %/d: {seven_rate}",
    );
    assert!(
        drain_reset_style(Some(five_rate), window_rate_unit(LABEL_5H), &five).is_some(),
        "a third-party 5h countdown must drain-color",
    );
    assert!(
        drain_reset_style(Some(seven_rate), window_rate_unit(LABEL_7D), &seven).is_some(),
        "a third-party 7d countdown must drain-color",
    );
}

/// A SEEDED third-party 5h window — the picked reading filled by the mirror the
/// scheduler runs on provider-derived windows — must still rate from the
/// window's own average pace: no third-party leg ever appends
/// `usage_history.jsonl`, so the recency-weighted branch would resolve no rate
/// at all and the countdown would lose the drain hue the bar-synthesized form
/// carries. The guard is the cache-family predicate, not `usage.is_none()`.
#[test]
fn drain_rate_seeded_third_party_window_keeps_avg_pace() {
    let _home = crate::testutil::HomeSandbox::new();
    let (p, mut p_readings) = third_party_profile(60.0, 30.0);
    p_readings.usage.insert(
        "tp".to_string(),
        UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 60.0,
                resets_at: Some(reset_in(9_000)),
            }),
            seven_day: None,
            ..Default::default()
        },
    );
    let (config, readings) = config_with(vec![(p, p_readings)], None, vec![]);
    let app = App::new(config);
    let profile = &app.config().profiles[0];
    let w = readings.usage.get("tp").unwrap().five_hour.clone().unwrap();
    let rate = drain_rate(
        &app,
        &crate::profile::ProfileName::from("tp"),
        profile,
        LABEL_5H,
        &w,
        &readings,
    )
    .expect("the avg pace answers for a seeded third-party window");
    // 60% over the 2.5h elapsed half of a 5h window is past its 50% ideal
    // line, so the cap applies: 70 / 3h ≈ 23.3 %/h, never `None`.
    assert!((rate - 23.333).abs() < 0.1, "5h rate in %/h: {rate}");
}

/// An OAuth 5h window keeps the recency-weighted recent burn, not the avg pace:
/// with no history recorded, it stays uncolored rather than falling back.
#[test]
fn drain_rate_oauth_five_hour_uses_recent_burn() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 60.0, 9_000);
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let app = App::new(config);
    let p = &app.config().profiles[0];
    let w = readings.usage.get("a").unwrap().five_hour.clone().unwrap();
    assert!(
        drain_rate(
            &app,
            &crate::profile::ProfileName::from("a"),
            p,
            LABEL_5H,
            &w,
            &readings
        )
        .is_none(),
        "no recorded history → no rate, rather than an avg-pace fallback",
    );
}

/// An api-key/provider profile: no `UsageInfo`, so its overview 5h/7d windows
/// are synthesized from the provider bars. Both bars sit exactly half-elapsed,
/// which is enough for `window_avg_pace_per_day` to have a pace to report.
fn third_party_profile(five_pct: f64, seven_pct: f64) -> (Profile, ReadingsSnapshot) {
    let bar = |label: &str, pct: f64, reset_secs: i64| crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some(reset_in(reset_secs)),
        used: None,
        total: None,
        inference_quota: false,
    };
    let stats = crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![
            bar(LABEL_5H, five_pct, 5 * 3600 / 2),
            bar(LABEL_7D, seven_pct, 7 * 86_400 / 2),
        ],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    (
        Profile {
            name: "tp".into(),
            base_url: Some("https://api.example.com".into()),
            api_key: Some("k".into()),
            auto_start: false,
            env: BTreeMap::new(),
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
            credentials: None,
            provider: None,
        },
        snap_with_stats(ReadingsSnapshot::default(), "tp", stats),
    )
}

/// A DeepSeek api-key profile with cached balance rows built from `totals`
/// (e.g. `["1.71 USD", "100.00 CNY"]`). Each total is preceded by a heading row
/// whose currency is extracted from the total value. An empty slice produces an
/// empty-rows snapshot, matching an account whose balance fetch has not landed.
fn deepseek_profile(name: &str, totals: &[&str]) -> (Profile, ReadingsSnapshot) {
    let mut rows = Vec::new();
    for t in totals {
        if let Some((_, currency)) = t.rsplit_once(' ') {
            rows.push(crate::providers::StatRow {
                label: format!("{currency} balance"),
                value: String::new(),
                kind: crate::providers::StatRowKind::Heading,
                raw: None,
            });
        }
        rows.push(crate::providers::StatRow {
            label: crate::providers::DEEPSEEK_BALANCE_ROW_LABEL.into(),
            value: (*t).to_string(),
            kind: crate::providers::StatRowKind::Body,
            raw: None,
        });
    }
    let stats = crate::providers::ThirdPartyStats {
        is_available: true,
        rows,
        bars: Vec::new(),
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    (
        Profile {
            name: name.into(),
            base_url: Some("https://api.deepseek.com/anthropic".into()),
            api_key: Some("k".into()),
            auto_start: false,
            env: BTreeMap::new(),
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
            credentials: None,
            provider: Some(crate::providers::Provider::DeepSeek),
        },
        snap_with_stats(ReadingsSnapshot::default(), name, stats),
    )
}

/// A DeepSeek profile whose cached rows come from a captured
/// `third_party_cache.json` (see the `CAPTURED_*_DS_CACHE` constants in
/// [`crate::testutil`]): the bytes go through the production cache writer and
/// reader into the field a live app's `apply_usage` fills, so the render path
/// is driven by captured bytes rather than a hand-built `ThirdPartyStats`.
fn deepseek_profile_from_cache(name: &str, captured: &str) -> (Profile, ReadingsSnapshot) {
    let (base, _) = deepseek_profile(name, &[]);
    // The cache writer skips names the on-disk record doesn't carry, so the
    // profile and the state list must exist before the captured bytes land.
    crate::profile::save_profile(&base).expect("save profile");
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec![name.into()],
        ..Default::default()
    })
    .expect("save state");
    crate::testutil::write_captured_third_party_cache(name, captured);
    let stats = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("captured cache written and readable");
    (
        Profile {
            name: name.into(),
            base_url: Some("https://api.deepseek.com/anthropic".into()),
            api_key: Some("k".into()),
            provider: Some(crate::providers::Provider::DeepSeek),
            ..base
        },
        snap_with_stats(ReadingsSnapshot::default(), name, stats),
    )
}

/// A chain-eligible OAuth profile with a live 5h window at `util`%, resetting
/// in `reset_secs`.
fn profile(name: &str, threshold: f64, util: f64, reset_secs: i64) -> (Profile, ReadingsSnapshot) {
    let usage = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: Some(reset_in(reset_secs)),
        }),
        ..UsageInfo::default()
    };
    (
        Profile {
            name: name.into(),
            base_url: None,
            api_key: None,
            auto_start: false,
            env: BTreeMap::new(),
            models: Default::default(),
            fallback_threshold: Some(threshold),
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
            credentials: None,
            provider: None,
        },
        snap_with_usage(ReadingsSnapshot::default(), name, usage),
    )
}

fn config_with(
    profiles: Vec<(Profile, ReadingsSnapshot)>,
    active: Option<&str>,
    chain: Vec<&str>,
) -> (AppConfig, ReadingsSnapshot) {
    let names: Vec<ProfileName> = profiles.iter().map(|(p, _)| p.name.clone()).collect();
    let mut readings = ReadingsSnapshot::default();
    for (_p, r) in &profiles {
        for (k, v) in &r.usage {
            readings.usage.insert(k.clone(), v.clone());
        }
        for (k, v) in &r.fetch_status {
            readings.fetch_status.insert(k.clone(), *v);
        }
        for (k, v) in &r.third_party {
            readings.third_party.insert(k.clone(), v.clone());
        }
    }
    let config = AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles: profiles.into_iter().map(|(p, _)| p).collect(),
    };
    (config, readings)
}

/// Flattens a line's spans to plain text for substring assertions.
fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The single row carrying the projected-switch `↲` hint, flattened to text.
/// Equality assertions pin the whole row (gauge, figures, eta), not just the
/// target name, so any hint-text or eta drift reds.
fn hinted_row_text(lines: &[Line<'static>]) -> String {
    let hinted: Vec<String> = lines
        .iter()
        .map(line_text)
        .filter(|t| t.contains('↲'))
        .collect();
    assert_eq!(
        hinted.len(),
        1,
        "exactly one row carries the projected-switch hint: {hinted:?}"
    );
    hinted.into_iter().next().expect("one hinted row")
}

fn resumes_line(lines: &[Line<'static>]) -> Option<String> {
    lines.iter().map(line_text).find(|t| t.contains("resumes:"))
}

// Wrap mode: the active profile itself is exhausted and stays put (no sink,
// the walk returns `None`) — previously silent. b resets sooner than a.
#[test]
fn all_exhausted_wrap_mode_shows_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 100.0, 3600);
    let (b, b_readings) = profile("b", 95.0, 100.0, 1800);
    let (config, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, &readings).0;
    let hint =
        resumes_line(&lines).expect("resumes hint must render when the whole chain is exhausted");
    assert!(
        hint.contains("resumes: b in ~"),
        "names the soonest-resuming member: {hint}"
    );
}

// Wrap-off: switch-off-all already cleared the active profile. The hint must
// not depend on an active profile being set at all.
#[test]
fn all_exhausted_wrap_off_active_cleared_shows_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 100.0, 900);
    let (b, b_readings) = profile("b", 95.0, 100.0, 3600);
    let (mut config, readings) =
        config_with(vec![(a, a_readings), (b, b_readings)], None, vec!["a", "b"]);
    config.state.switch_off_when_spent = true;
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, &readings).0;
    let hint = resumes_line(&lines)
        .expect("resumes hint must render even with no active profile (wrap-off cleared it)");
    assert!(hint.contains("resumes: a in ~"), "{hint}");
}

// b still has headroom — the chain is not all-exhausted, so the caption must
// stay hidden (recovery would relink b on the next tick regardless).
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 100.0, 3600);
    let (b, b_readings) = profile("b", 95.0, 20.0, 3600);
    let (config, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, &readings).0;
    assert!(
        resumes_line(&lines).is_none(),
        "must not show when the chain isn't fully exhausted"
    );
}

/// A wallet-bearing active shows its runway beside the chain caption: the
/// funded balance, its burn rate, and however long the two hold — the wallet
/// sibling of the switch projection. No threshold, no warning hue; the
/// operator judges the figure.
#[test]
fn a_wallet_bearing_active_shows_its_drains_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let (ds, ds_readings) = deepseek_profile("ds", &["63.34 CNY"]);
    let (config, readings) = config_with(vec![(ds, ds_readings)], Some("ds"), vec!["ds"]);
    let mut app = App::new(config);
    let now = crate::usage::now_ms();
    // A dense hourly series for the funded wallet, falling 4.5 CNY/h —
    // chronological, the order `load_wallet_history` hands the cache.
    app.wallet_cache.insert(
        "ds".to_string(),
        (1..=12u64)
            .rev()
            .map(|hours_ago| crate::usage::WalletSample {
                ts: now - hours_ago * 3_600_000,
                label: "api balance".to_string(),
                amount: 63.34 + 4.5 * hours_ago as f64,
                currency: "CNY".to_string(),
            })
            .collect(),
    );
    let lines = fallback_flow_lines(&app, 60, &readings).0;
    let drains = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("drains in ~"))
        .expect("a wallet-bearing active shows its runway");
    assert!(
        drains.contains("api balance drains in ~"),
        "the line names the wallet it measures: {drains}"
    );
}

/// An active with no balance series shows no drains line — a cold series is
/// not a runway claim.
#[test]
fn a_wallet_bearing_active_without_a_series_shows_no_drains_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let (ds, ds_readings) = deepseek_profile("ds", &["63.34 CNY"]);
    let (config, readings) = config_with(vec![(ds, ds_readings)], Some("ds"), vec!["ds"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, &readings).0;
    assert!(
        !lines.iter().map(line_text).any(|t| t.contains("drains in")),
        "no series, no runway: {:?}",
        lines.iter().map(line_text).collect::<Vec<_>>()
    );
}

// Nobody near their threshold at all — the ordinary healthy-chain case.
#[test]
fn healthy_chain_hides_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (b, b_readings) = profile("b", 95.0, 5.0, 3600);
    let (config, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, &readings).0;
    assert!(resumes_line(&lines).is_none());
}

/// \#116 review: the Overview's chain rows carry the blocked-reason markers the
/// Fallback rows do, so a member clauth cannot measure reads `◆` here too. The
/// arm had no render pin, which means it could have been deleted and every test
/// would have stayed green. The Overview's ACCOUNT rows carry no
/// blocked-reason mark at all, which is why the wiki names the chain rows.
#[test]
fn an_unmeasured_chain_member_is_marked_on_the_overview_chain_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    // No reading for `blind`: nothing polls a member whose sessions run on a
    // long-lived token with no OAuth pair behind it.
    let rows = |armed: bool| {
        let (ok, ok_readings) = profile("ok", 95.0, 10.0, 3600);
        let (blind, _unpolled) = profile("blind", 95.0, 0.0, 3600);
        let (config, readings) = config_with(
            vec![(ok, ok_readings), (blind, ReadingsSnapshot::default())],
            Some("ok"),
            vec!["ok", "blind"],
        );
        let mut app = App::new(config);
        if armed {
            app.session_tokens.insert(
                "blind".into(),
                crate::claude::SessionTokenStatus::LongLived(None),
            );
        }
        fallback_flow_lines(&app, 60, &readings).0
    };

    let lines = rows(true);
    let row = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("blind"))
        .expect("the chain row for the unmeasured member");
    assert!(row.contains('◆'), "the row carries the marker: {row:?}");
    let marker = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content == "◆")
        .expect("the marker span");
    assert_eq!(
        marker.style.fg,
        theme::warning().fg,
        "at the warning hue, not stale's faint"
    );
    assert!(
        !rows(false).iter().map(line_text).any(|t| t.contains('◆')),
        "the same absent reading without a session token is the logged-out shape"
    );
}

// ── overview row state cues ──────────────────────────────────────────────

/// Marker column: a broken login (×) outranks both the bell (!) and the
/// active dot (●) — usage alerts are moot until re-login.
#[test]
fn broken_login_marker_outranks_bell_and_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (mut config, readings) = config_with(vec![(a, a_readings)], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.bell_fired.observe("a", Some(50.0), Some(60.0), true);
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let text = line_text(&line);
    assert!(text.contains('×'), "broken login renders ×: {text}");
    assert!(!text.contains('!'), "bell yields to ×: {text}");
    assert!(!text.contains('●'), "active dot yields to ×: {text}");
    let marker = line.spans.iter().find(|s| s.content == "×").unwrap();
    assert_eq!(marker.style.fg, theme::danger().fg);
}

#[test]
fn bell_marker_shows_when_login_is_fine() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let mut app = App::new(config);
    app.bell_fired.observe("a", Some(50.0), Some(60.0), true);
    let widths = OverviewWidths::new(80, &app, &readings);
    let text = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(text.contains('!'), "{text}");
    assert!(!text.contains('×'), "{text}");
}

/// A dead / mis-filled long-lived token (⊘) outranks bell (!) and active (●):
/// the next switch would sign sessions out, so it beats a usage alert.
#[test]
fn token_danger_marker_outranks_bell_and_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (config, readings) = config_with(vec![(a, a_readings)], Some("a"), vec![]); // active
    let mut app = App::new(config);
    app.bell_fired.observe("a", Some(50.0), Some(60.0), true); // bell also fired
    app.session_tokens
        .insert("a".into(), crate::claude::SessionTokenStatus::NotLongLived);
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let text = line_text(&line);
    assert!(text.contains('⊘'), "mis-filled token renders ⊘: {text}");
    assert!(!text.contains('!'), "bell yields to ⊘: {text}");
    assert!(!text.contains('●'), "active dot yields to ⊘: {text}");
    let marker = line.spans.iter().find(|s| s.content == "⊘").unwrap();
    assert_eq!(marker.style.fg, theme::danger().fg);
}

/// A canceled subscription (⊖) is dead-first: the org 403s every request, so it
/// outranks the broken-login ×, the token ⊘, the bell !, and the active ● all at
/// once (matching the Fallback ladder where `Canceled` beats `AuthBroken`). The
/// auth_broken + bell + active fixture proves the canceled arm fires FIRST — if it
/// yielded, the × would show instead. `⊖` is shared with `Disabled` and split on
/// hue, so the danger assertion below is what pins the canceled arm.
#[test]
fn canceled_marker_is_dead_first() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::usage::{PlanInfo, PlanTier};
    let (a, mut a_readings) = profile("a", 95.0, 10.0, 3600);
    a_readings.usage.get_mut("a").unwrap().plan = Some(PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
        codex_plan: None,
        email: None,
    });
    let (mut config, readings) = config_with(vec![(a, a_readings)], Some("a"), vec![]); // also active
    config.state.auth_broken.push("a".into()); // also auth-broken
    let mut app = App::new(config);
    app.bell_fired.observe("a", Some(50.0), Some(60.0), true); // bell also fired
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let text = line_text(&line);
    assert!(text.contains('⊖'), "canceled renders ⊖: {text}");
    assert!(!text.contains('×'), "broken login yields to ⊖: {text}");
    assert!(!text.contains('!'), "bell yields to ⊖: {text}");
    assert!(!text.contains('●'), "active dot yields to ⊖: {text}");
    let marker = line.spans.iter().find(|s| s.content == "⊖").unwrap();
    assert_eq!(marker.style.fg, theme::danger().fg);
}

/// But a broken login (×) still wins over a token-danger marker.
#[test]
fn broken_login_outranks_token_danger_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (mut config, readings) = config_with(vec![(a, a_readings)], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.session_tokens
        .insert("a".into(), crate::claude::SessionTokenStatus::NotLongLived);
    let widths = OverviewWidths::new(80, &app, &readings);
    let text = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(text.contains('×'), "broken login wins: {text}");
    assert!(!text.contains('⊘'), "token marker yields to ×: {text}");
}

/// A live long-lived token raises no marker; an expired one raises the ⊘ danger marker.
#[test]
fn long_lived_token_expired_marks() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::claude::SessionTokenStatus as S;
    let day = 86_400_000_i64;
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let mut app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);

    app.session_tokens
        .insert("a".into(), S::LongLived(Some(now_ms() as i64 + 340 * day)));
    let live = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(
        !live.contains('⊘'),
        "a live token raises no danger marker: {live}"
    );

    app.session_tokens
        .insert("a".into(), S::LongLived(Some(now_ms() as i64 - day)));
    let dead = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(dead.contains('⊘'), "expired token raises ⊘: {dead}");
}

/// The stale-data cue lives on the refresh countdown now — an underlined name
/// would double-signal, and the bar brackets stay plain dim.
#[test]
fn cached_row_colors_countdown_amber_and_underlines_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut a, mut a_readings) = profile("a", 95.0, 10.0, 3600);
    // A login puts the account on the OAuth leg's work list; a logged-out one
    // carries the not-current mark in this slot instead of a countdown.
    a.credentials = Some(oauth_creds());
    a_readings
        .fetch_status
        .insert("a".to_string(), FetchStatus::Cached);
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile.lock().unwrap().insert(
        FetchLeg::OAuth.key(ProfileName::from("a")),
        now_ms() + 30_000,
    );
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    assert!(
        line.spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "underline cue is retired"
    );
    let bracket = line
        .spans
        .iter()
        .find(|s| s.content == "[")
        .expect("bracketed 5h bar");
    assert_eq!(bracket.style.fg, theme::dim().fg, "brackets stay plain dim");
    let countdown = line
        .spans
        .iter()
        .find(|s| s.content.ends_with("s "))
        .expect("refresh countdown");
    assert_eq!(countdown.style.fg, Some(theme::warning_color()));
}

#[test]
fn failed_row_colors_countdown_red() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut a, mut a_readings) = profile("a", 95.0, 10.0, 3600);
    a.credentials = Some(oauth_creds());
    a_readings
        .fetch_status
        .insert("a".to_string(), FetchStatus::Failed);
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile.lock().unwrap().insert(
        FetchLeg::OAuth.key(ProfileName::from("a")),
        now_ms() + 30_000,
    );
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let bracket = line
        .spans
        .iter()
        .find(|s| s.content == "[")
        .expect("bracketed 5h bar");
    assert_eq!(bracket.style.fg, theme::dim().fg, "brackets stay plain dim");
    let countdown = line
        .spans
        .iter()
        .find(|s| s.content.ends_with("s "))
        .expect("refresh countdown");
    assert_eq!(countdown.style.fg, Some(theme::danger_color()));
}

/// Every `(reset)` countdown suffix on a row, in column order.
fn reset_suffixes(line: &Line<'static>) -> Vec<Span<'static>> {
    line.spans
        .iter()
        .filter(|s| s.content.starts_with(" (") && s.content.ends_with(')'))
        .cloned()
        .collect()
}

/// The wiring, end to end. Both call sites used to pass a hardcoded `None` for
/// an api-key profile (no `UsageInfo` → no burn history → no rate), so a
/// third-party row's countdowns stayed faint however fast the window drained.
#[test]
fn third_party_row_drain_colors_both_countdowns() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (config, readings) = config_with(vec![third_party_profile(60.0, 30.0)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app, &readings);
    assert!(
        widths.five_hour >= 26 && widths.seven_day >= 26,
        "test needs both columns wide enough to render a (reset) suffix",
    );
    let suffixes = reset_suffixes(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert_eq!(suffixes.len(), 2, "both windows render a (reset) suffix");
    for s in suffixes {
        assert_ne!(
            s.style.fg,
            theme::faint().fg,
            "a synthesized third-party window must still drain-color: {:?}",
            s.content,
        );
    }
}

/// The 7d half for an OAuth profile: its countdown drains off the window's own
/// average pace, so it colors even though the 5h burn history is empty.
#[test]
fn oauth_row_drain_colors_the_seven_day_countdown() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (a, mut a_readings) = profile("a", 95.0, 60.0, 9_000);
    a_readings.usage.get_mut("a").unwrap().seven_day = Some(UsageWindow {
        utilization: 30.0,
        resets_at: Some(reset_in(7 * 86_400 / 2)),
    });
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app, &readings);
    let suffixes = reset_suffixes(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert_eq!(suffixes.len(), 2);
    assert_eq!(
        suffixes[0].style.fg,
        theme::faint().fg,
        "5h keeps the recent-burn source, which has no history here",
    );
    assert_ne!(
        suffixes[1].style.fg,
        theme::faint().fg,
        "7d drains off its own avg pace",
    );
}

/// Gap widening must work from the row's REAL width. `fixed_overview_width`
/// omits the TIMER_SLOT the row always renders, and widening gaps from that
/// undercounted figure overflows the row at narrow widths, clipping the tail
/// of the 5h column (observed at a 50-column pane: `[░░░░░]  0` with the `%`
/// pushed off-screen). Whenever the columns fit at all at minimum gaps, the
/// gap-widened layout must still fit.
#[test]
fn gap_widening_never_clips_the_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("ax-main", 95.0, 10.0, 3600);
    let (b, b_readings) = profile("ax-backup", 95.0, 20.0, 3600);
    let (config, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("ax-main"),
        vec![],
    );
    let app = App::new(config);
    for width in 34u16..=200 {
        let w = OverviewWidths::new(width, &app, &readings);
        let min =
            fixed_overview_width(w.name, w.kind, w.five_hour, w.seven_day, w.live, 2) + TIMER_SLOT;
        if min > width as usize {
            // Below this the shrink loop has already bottomed out and the row
            // deliberately overflows-and-clips; gap widening isn't the cause.
            continue;
        }
        let used = fixed_overview_width(w.name, w.kind, w.five_hour, w.seven_day, w.live, w.gap)
            + TIMER_SLOT;
        assert!(
            used <= width as usize,
            "row overflows at width {width}: used {used} (gap {})",
            w.gap
        );
    }
}

/// A credentialed OAuth profile with no fetched `usage.plan` yet, so
/// `account_type_label` falls back to the token's `subscription_type` via
/// `PlanTier::from_subscription_type(..).display()`.
fn credentialed_profile(name: &str, subscription_type: &str) -> (Profile, ReadingsSnapshot) {
    (
        Profile {
            name: name.into(),
            base_url: None,
            api_key: None,
            auto_start: false,
            env: BTreeMap::new(),
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
            credentials: Some(ClaudeCredentials {
                claude_ai_oauth: Some(OAuthToken {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    scopes: None,
                    subscription_type: Some(subscription_type.into()),
                    ..crate::profile::OAuthToken::default_extra()
                }),
            }),
            provider: None,
        },
        ReadingsSnapshot::default(),
    )
}

/// The credentialed pulse arm must clamp the type-column label to
/// `widths.kind` exactly like the non-credentialed `fixed` arm does. A
/// long label ("Enterprise", 10 chars) must not overflow a narrow `kind`
/// column (6 chars) and bleed into the following gap/timer columns.
#[test]
fn credentialed_long_label_clamps_to_kind_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = credentialed_profile("acct", "enterprise");
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(60, &app, &readings);
    assert_eq!(
        widths.kind, 6,
        "test assumes a 6-wide kind column at this pane width"
    );

    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let chars: Vec<char> = line_text(&line).chars().collect();

    // 2 = cursor slot, 2 = marker slot (both always exactly 2 chars).
    let start = 2 + 2 + widths.name + widths.gap;
    let kind_field: String = chars[start..start + widths.kind].iter().collect();
    assert_eq!(
        kind_field, "Enter…",
        "type column must truncate+pad to exactly `kind` width"
    );
    assert_eq!(
        chars[start + widths.kind],
        ' ',
        "type column must not bleed into the following gap/timer columns"
    );
}

// ── disabled accounts (feature: per-account disable toggle) ──────────────

/// A disabled account's row dims its name (never `name_color`'s active/
/// inactive branch — a disabled account can never be active) and that is the
/// row's ONLY change: the `TYPE` column keeps its real oauth/api/tier value,
/// since a non-type value under a `TYPE` header both lies about the column and
/// destroys the tier the operator came to read. The label itself lives on the
/// Usage `status` row and the Setup `status` row. A sibling enabled row in the
/// same config proves the dimming is per-profile, not global.
#[test]
fn disabled_row_dims_its_name_and_keeps_the_real_type_value() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut a, mut a_readings) = profile("a", 95.0, 10.0, 3600);
    a.disabled = true;
    let (b, mut b_readings) = profile("b", 95.0, 10.0, 3600);
    // Give both a real FETCHED Pro tier so the TYPE column reads a genuine tier —
    // this test is about dimming keeping whatever tier is real, not about the
    // fallback tier a credential-less profile happens to default to.
    let plan = crate::usage::PlanInfo {
        tier: crate::usage::PlanTier::Pro,
        subscription_status: None,
        codex_plan: None,
        email: None,
    };
    a_readings.usage.get_mut("a").unwrap().plan = Some(plan.clone());
    b_readings.usage.get_mut("b").unwrap().plan = Some(plan);
    let (config, readings) = config_with(vec![(a, a_readings), (b, b_readings)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(80, &app, &readings);

    let disabled_line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let name_span = disabled_line
        .spans
        .iter()
        .find(|s| s.content.trim_end() == "a")
        .expect("name span renders");
    assert_eq!(
        name_span.style.fg,
        theme::dim().fg,
        "a disabled account's name renders dim, not the active/inactive name_color"
    );

    let enabled_line = render_overview_row(&app, 1, &widths, false, true, &readings);
    let enabled_name_span = enabled_line
        .spans
        .iter()
        .find(|s| s.content.trim_end() == "b")
        .expect("name span renders");
    assert_ne!(
        enabled_name_span.style.fg,
        theme::dim().fg,
        "an enabled account keeps its ordinary name color"
    );

    // Same profile shape either side of the `disabled` bit, so the type column
    // must read identically — and must be the real tier, not an empty slot.
    let kind_field = |line: &Line<'static>| -> String {
        let chars: Vec<char> = line_text(line).chars().collect();
        let start = 2 + 2 + widths.name + widths.gap;
        chars[start..start + widths.kind].iter().collect()
    };
    let disabled_kind = kind_field(&disabled_line);
    assert_eq!(
        disabled_kind.trim(),
        "Pro",
        "the disabled row keeps its real tier value in the TYPE column"
    );
    assert_eq!(
        disabled_kind,
        kind_field(&enabled_line),
        "the `disabled` bit changes nothing about the TYPE column"
    );
    assert!(
        !line_text(&disabled_line).contains("disabled"),
        "no chip anywhere on the row: {}",
        line_text(&disabled_line)
    );
}

/// A credentialed OAuth profile, so the type cell takes the pulsing branch.
fn oauth_creds() -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// A disabled row goes inert END TO END, not just its name: the marker glyph,
/// the type cell, and both window bars all flatten to `theme::dim()`. The
/// glyphs and numbers stay — the row never lets state ride on hue alone, and
/// the figures are the last real reading — it is only the semantic color that
/// lies once the data is frozen. An enabled sibling in the same config keeps
/// every hue, which is what proves the flattening is per-row.
#[test]
fn disabled_row_flattens_every_semantic_hue_to_dim() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut a, a_readings) = profile("a", 95.0, 90.0, 3600);
    a.disabled = true;
    a.credentials = Some(oauth_creds());
    let (mut b, b_readings) = profile("b", 95.0, 90.0, 3600);
    b.credentials = Some(oauth_creds());
    // Both rows must actually REACH the marker branch, or the flattening
    // assertion silently skips it: a disabled, non-active, unbroken profile
    // falls through to a blank marker with no fg to flatten. Marking both
    // auth-broken puts the `×` (DANGER) on each.
    let (mut config, readings) = config_with(vec![(a, a_readings), (b, b_readings)], None, vec![]);
    config.state.auth_broken.push("a".into());
    config.state.auth_broken.push("b".into());
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, &readings);

    let non_dim = |line: &Line<'static>| -> Vec<String> {
        line.spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .filter(|s| s.style.fg.is_some() && s.style.fg != theme::dim().fg)
            .map(|s| s.content.trim().to_string())
            .collect()
    };

    // The one hue a disabled row keeps is the not-current mark: it exists to say
    // the dim figures beside it are a cached reading, so flattening it would
    // erase the only cue that they are not current.
    let disabled_line = render_overview_row(&app, 0, &widths, false, true, &readings);
    assert_eq!(
        non_dim(&disabled_line),
        vec![NOT_CURRENT_MARK.to_string()],
        "every colored span on a disabled row flattens to dim but the not-current mark"
    );

    // The control row must still carry hue, or the assertion above is vacuous.
    let enabled_line = render_overview_row(&app, 1, &widths, false, true, &readings);
    assert!(
        !non_dim(&enabled_line).is_empty(),
        "control: an enabled row keeps its semantic colors"
    );

    // The bar figures survive the flattening — dim, not deleted.
    let text: String = disabled_line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("90%"),
        "the last-known reading stays readable: {text}"
    );
}

/// The credentialed type cell's identity-wave is ambient motion, which reads as
/// "live". A disabled row must render a flat cell instead — so two frames at
/// different `started_at` elapsed values must be byte-and-style identical. A
/// surviving pulse would differ between them.
#[test]
fn disabled_row_type_cell_does_not_pulse() {
    let _home = crate::testutil::HomeSandbox::new();
    // The wave is a Full-tier effect: `pulse_name_spans` returns flat spans
    // below it. A test build's tier defaults to `Full`, and the sandbox pins it
    // explicitly, so the wave renders and the disabled-vs-enabled control below
    // is real.
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let (mut a, a_readings) = profile("a", 95.0, 10.0, 3600);
    a.disabled = true;
    a.credentials = Some(oauth_creds());
    let (mut b, b_readings) = profile("b", 95.0, 10.0, 3600);
    b.credentials = Some(oauth_creds());
    let (config, readings) = config_with(vec![(a, a_readings), (b, b_readings)], None, vec![]);

    // `pulse_name_spans` keys off `App::anim_ms`, so two Apps with different
    // pinned phases sample different points of the wave. 450ms is the crest of
    // the 900ms sweep, where the envelope peaks: the phase furthest from the
    // flat 0ms frame, so a surviving pulse shows up at its widest rather than at
    // some near-zero lean that rounds back to base.
    let snapshot = |idx: usize| -> Vec<(String, Option<ratatui::style::Color>)> {
        let mut app = App::new(config.clone());
        app.anim_phase_ms = Some(450 * idx as u64);
        let widths = OverviewWidths::new(110, &app, &readings);
        render_overview_row(&app, 0, &widths, false, true, &readings)
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    };
    assert_eq!(
        snapshot(0),
        snapshot(1),
        "a disabled row must render identically at two wave phases (no pulse)"
    );

    // Control: the ENABLED sibling does pulse, so the comparison above is real.
    let enabled_snapshot = |elapsed_ms: u64| -> Vec<Option<ratatui::style::Color>> {
        let mut app = App::new(config.clone());
        app.anim_phase_ms = Some(elapsed_ms);
        let widths = OverviewWidths::new(110, &app, &readings);
        render_overview_row(&app, 1, &widths, false, true, &readings)
            .spans
            .iter()
            .map(|s| s.style.fg)
            .collect()
    };
    assert_ne!(
        enabled_snapshot(0),
        enabled_snapshot(450),
        "control: an enabled credentialed row's type cell really does animate"
    );
}

/// The type cell's index in a row's span list: cursor, marker, name, name pad,
/// gap, then the cell. Read positionally on purpose — identifying it by content
/// would collide with the 5h/7d cells, which render the same `—` whenever a
/// profile has no usage, which is exactly the fixture these tests use.
const KIND_SPAN: usize = 5;

/// A no-data dash is not a tier, so the identity wave must not carry it: a lone
/// glyph color-cycling next to the row's static faint dashes reads as live data.
/// Same two-phase shape as the disabled-row pin above, and the same reason it
/// needs a control — without one, a harness that never animates anything passes
/// this as "no pulse".
#[test]
fn no_tier_type_cell_does_not_pulse() {
    let _home = crate::testutil::HomeSandbox::new();
    // The wave is Full-tier only; the test build defaults to `Full` and the
    // sandbox pins it explicitly. Same guard the disabled-row pin carries.
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    // `something_new` is a claim clauth cannot classify, so the row has no tier
    // at all; `max` is the same row WITH one.
    let (config, readings) = config_with(
        vec![
            credentialed_profile("a", "something_new"),
            credentialed_profile("b", "max"),
        ],
        None,
        vec![],
    );

    let snapshot = |idx: usize, phase_ms: u64| -> Vec<(String, Option<ratatui::style::Color>)> {
        let mut app = App::new(config.clone());
        app.anim_phase_ms = Some(phase_ms);
        let widths = OverviewWidths::new(110, &app, &readings);
        render_overview_row(&app, idx, &widths, false, true, &readings)
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    };

    // NOT 450ms, the crest the disabled-row pin above uses. That phase is the
    // one value a ONE-CHARACTER label cannot express. `pulse_name_spans` weights
    // char `i` by `crest = ((col − head).cos() · 0.5 + 0.5)²` — note the remap,
    // which is what turns the cosine's −1 into a 0 rather than a trough. A lone
    // char sits at `col = 0`, and at progress 0.5 `head` is half a turn, so
    // `crest = ((−1) · 0.5 + 0.5)² = 0`: no tint, landing on the same base color
    // as the flat 0ms frame, whose `envelope = sin(0)` is 0 for its own reason.
    // A `—` cell would then compare equal at both phases whether or not it
    // pulsed, and the mutation deleting the guard stays green. 135ms is near the
    // peak `crest × envelope` for one char, where the tint is observable.
    assert_eq!(
        snapshot(0, 0),
        snapshot(0, 135),
        "a no-tier row must render identically at two wave phases (no pulse)"
    );
    assert_ne!(
        snapshot(1, 0),
        snapshot(1, 135),
        "control: a row that HAS a tier really does animate, so the equality above is real"
    );
}

/// The dash joins the other no-data cells at `faint`, but only when it is the
/// whole cell: a disabled row flattens to `dim` the way it outranks every other
/// cell state, and a row whose label is a REAL one keeps `dim` however it
/// reached the un-pulsed branch.
///
/// That last leg is the one the `no_tier &&` conjunct exists for. An api-key row
/// has a genuine `API` label AND no credentials, so it lands in the same branch
/// a no-data dash does; without the conjunct every DeepSeek / Z.ai row fades.
#[test]
fn no_tier_type_cell_reads_faint_unless_something_real_shares_the_cell() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let (mut disabled, _disabled_readings) = credentialed_profile("c", "something_new");
    disabled.disabled = true;
    let (config, readings) = config_with(
        vec![
            credentialed_profile("a", "something_new"),
            (disabled, _disabled_readings),
            (
                Profile::new(
                    "d".to_string(),
                    Some("https://api.deepseek.com/anthropic".to_string()),
                    Some("sk-fixture".to_string()),
                ),
                ReadingsSnapshot::default(),
            ),
            (
                Profile::new("e".to_string(), None, None),
                ReadingsSnapshot::default(),
            ),
        ],
        None,
        vec![],
    );

    let cell = |idx: usize| -> (String, Option<ratatui::style::Color>) {
        let mut app = App::new(config.clone());
        app.anim_phase_ms = Some(0);
        let widths = OverviewWidths::new(110, &app, &readings);
        let span = render_overview_row(&app, idx, &widths, false, true, &readings).spans[KIND_SPAN]
            .clone();
        (span.content.to_string(), span.style.fg)
    };

    let (bare, bare_fg) = cell(0);
    assert_eq!(
        bare.trim_end(),
        "—",
        "fixture control: the cell is the dash"
    );
    assert_eq!(bare_fg, theme::faint().fg, "a bare dash is a no-data cell");

    let (_, disabled_fg) = cell(1);
    assert_eq!(
        disabled_fg,
        theme::dim().fg,
        "a disabled row flattens to dim, outranking no-data as it does stale"
    );

    let (api, api_fg) = cell(2);
    assert_eq!(
        api.trim_end(),
        "API",
        "fixture control: an api-key row carries a real label, not the dash"
    );
    assert_eq!(
        api_fg,
        theme::dim().fg,
        "a real label never fades, however it reached the un-pulsed branch"
    );

    // An UNCREDENTIALED account reaches the same branch by a different route
    // (no credentials rather than no tier), and it fades too — the cell is empty
    // for the same reason, so it reads the same way. This one changed with the
    // no-data dash and had no leg of its own.
    let (uncredentialed, uncredentialed_fg) = cell(3);
    assert_eq!(
        uncredentialed.trim_end(),
        "—",
        "fixture control: an uncredentialed oauth row has no tier to show"
    );
    assert_eq!(
        uncredentialed_fg,
        theme::faint().fg,
        "a bare dash is a no-data cell whether or not credentials exist"
    );
}

/// A disabled account is never polled, so its refresh countdown would tick to
/// zero and then claim a refresh forever. The slot shows no countdown — at full
/// width, so no column downstream shifts.
#[test]
fn disabled_row_claims_no_refresh_at_full_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut a, a_readings) = profile("a", 95.0, 10.0, 3600);
    a.disabled = true;
    let (mut b, b_readings) = profile("b", 95.0, 10.0, 3600);
    b.credentials = Some(oauth_creds());
    let (config, readings) = config_with(vec![(a, a_readings), (b, b_readings)], None, vec![]);
    let app = App::new(config);
    // Both profiles carry a live countdown in the shared map.
    if let Ok(mut m) = app.next_refresh_per_profile.lock() {
        m.insert(
            FetchLeg::OAuth.key(ProfileName::from("a")),
            now_ms() + 42_000,
        );
        m.insert(
            FetchLeg::OAuth.key(ProfileName::from("b")),
            now_ms() + 42_000,
        );
    }
    let widths = OverviewWidths::new(110, &app, &readings);

    let disabled_line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let enabled_line = render_overview_row(&app, 1, &widths, false, true, &readings);
    let text =
        |l: &Line<'static>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };

    // Match the countdown by SHAPE (a digit followed by `s`), never by its exact
    // value: the seconds figure truncates, so a literal "42s" flips to "41s" the
    // moment a millisecond passes between the insert and the render. Nothing
    // else on the row can produce that pair — the bar, `10%` and `(1h 0m)` carry
    // no `s`, and the names are single letters.
    let has_countdown = |s: &str| -> bool {
        s.as_bytes()
            .windows(2)
            .any(|w| w[0].is_ascii_digit() && w[1] == b's')
    };
    // The control proves the countdown is genuinely reachable in this fixture —
    // without it a blank slot would pass even if the timer never rendered.
    assert!(
        has_countdown(&text(&enabled_line)),
        "control: the enabled row shows its countdown: {}",
        text(&enabled_line)
    );
    assert!(
        !has_countdown(&text(&disabled_line)),
        "the disabled row claims no refresh: {}",
        text(&disabled_line)
    );

    // Width is preserved, so nothing downstream shifts: the two rows must be
    // exactly as wide as each other.
    assert_eq!(
        text(&disabled_line).chars().count(),
        text(&enabled_line).chars().count(),
        "blanking the timer must not collapse the slot"
    );
}

// ── not-current mark: an unpolled row drawing a cached reading ────────────

/// The glyph an unpolled row that draws a cached reading carries in its
/// countdown slot.
const NOT_CURRENT_MARK: &str = "⋯";

/// The spans on a row whose trimmed content is the not-current mark.
fn mark_spans(line: &Line<'static>) -> Vec<Span<'static>> {
    line.spans
        .iter()
        .filter(|s| s.content.trim() == NOT_CURRENT_MARK)
        .cloned()
        .collect()
}

/// A refresh countdown matched by shape (a digit followed by `s`); see
/// `disabled_row_claims_no_refresh_at_full_width` for why never by value.
fn has_refresh_countdown(line: &Line<'static>) -> bool {
    line_text(line)
        .as_bytes()
        .windows(2)
        .any(|w| w[0].is_ascii_digit() && w[1] == b's')
}

/// A Z.ai account with no api key and cached 5h/7d bars, named `tp`: no fetch
/// leg's work list holds a keyless provider account, so its bars are the last
/// reading clauth cached.
fn keyless_provider_profile() -> (Profile, ReadingsSnapshot) {
    let (mut p, readings) = third_party_profile(40.0, 20.0);
    p.base_url = Some("https://api.z.ai/api/anthropic".into());
    p.api_key = None;
    p.provider = Some(crate::providers::Provider::Zai);
    (p, readings)
}

/// Every way an account falls off both fetch legs' work lists while its row
/// still draws a reading: disabled (OAuth bars, DeepSeek balance), keyless
/// provider, logged-out OAuth. Each carries one amber `⋯` where the countdown
/// sits, at both theme tiers, and keeps the row's width. A polled sibling with
/// a scheduled refresh keeps its countdown and gets no mark.
#[test]
fn an_unpolled_row_drawing_a_reading_carries_an_amber_not_current_mark() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut disabled_oauth, disabled_oauth_readings) = profile("a", 95.0, 10.0, 3600);
    disabled_oauth.disabled = true;
    disabled_oauth.credentials = Some(oauth_creds());
    // `profile` builds a credential-less OAuth account: logged out.
    let logged_out = profile("c", 95.0, 10.0, 3600);
    let (mut disabled_deepseek, disabled_deepseek_readings) = deepseek_profile("d", &["1.71 USD"]);
    disabled_deepseek.disabled = true;
    let (mut polled, polled_readings) = profile("p", 95.0, 10.0, 3600);
    polled.credentials = Some(oauth_creds());
    let (config, readings) = config_with(
        vec![
            (disabled_oauth, disabled_oauth_readings),
            keyless_provider_profile(),
            logged_out,
            (disabled_deepseek, disabled_deepseek_readings),
            (polled, polled_readings),
        ],
        None,
        vec![],
    );
    let unpolled = [
        (0, "disabled oauth"),
        (1, "keyless provider"),
        (2, "logged-out oauth"),
        (3, "disabled deepseek balance"),
    ];
    let polled_idx = 4;

    let mut mark_fg_per_tier = Vec::new();
    for tier in [theme::Tier::Full, theme::Tier::Compatible] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        let app = App::new(config.clone());
        // Every row holds a scheduled refresh in the shared map, so only the
        // work-list rule can keep a countdown off the unpolled rows.
        if let Ok(mut m) = app.next_refresh_per_profile.lock() {
            for name in ["a", "c", "p"] {
                m.insert(
                    FetchLeg::OAuth.key(ProfileName::from(name)),
                    now_ms() + 42_000,
                );
            }
            for name in ["tp", "d"] {
                m.insert(
                    FetchLeg::ThirdParty.key(ProfileName::from(name)),
                    now_ms() + 42_000,
                );
            }
        }
        let widths = OverviewWidths::new(110, &app, &readings);
        let row = |idx: usize| render_overview_row(&app, idx, &widths, false, true, &readings);

        let control = row(polled_idx);
        assert!(
            has_refresh_countdown(&control),
            "control ({tier:?}): the polled row shows its countdown: {}",
            line_text(&control)
        );
        assert!(
            mark_spans(&control).is_empty(),
            "a polled row carries no not-current mark ({tier:?}): {}",
            line_text(&control)
        );

        for (idx, what) in unpolled {
            let line = row(idx);
            let marks = mark_spans(&line);
            assert_eq!(
                marks.len(),
                1,
                "a {what} row drawing a reading carries one `⋯` ({tier:?}): {}",
                line_text(&line)
            );
            assert_eq!(
                marks[0].style.fg,
                Some(theme::warning_color()),
                "the {what} row's `⋯` is amber ({tier:?})"
            );
            assert!(
                !has_refresh_countdown(&line),
                "the {what} row claims no refresh ({tier:?}): {}",
                line_text(&line)
            );
            assert_eq!(
                line.width(),
                control.width(),
                "the {what} row's mark keeps the slot's width in cells ({tier:?})"
            );
        }
        mark_fg_per_tier.push(theme::warning_color());
    }
    assert_ne!(
        mark_fg_per_tier[0], mark_fg_per_tier[1],
        "control: the two tier pins resolve different colors, so both legs ran"
    );
}

/// The mark says the figures beside it are old; a row with nothing cached has
/// no figures, so its slot stays blank however unpolled the account is. A
/// polled account whose first refresh is not scheduled yet gets no mark either:
/// a leg will fetch it.
#[test]
fn a_row_with_no_reading_or_a_polled_account_gets_no_not_current_mark() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let (mut disabled_empty, _) = profile("a", 95.0, 10.0, 3600);
    disabled_empty.disabled = true;
    disabled_empty.credentials = Some(oauth_creds());
    let (keyless_empty, _) = keyless_provider_profile();
    let (logged_out_empty, _) = profile("c", 95.0, 10.0, 3600);
    let (mut polled_unscheduled, polled_readings) = profile("p", 95.0, 10.0, 3600);
    polled_unscheduled.credentials = Some(oauth_creds());
    let (config, readings) = config_with(
        vec![
            (disabled_empty, ReadingsSnapshot::default()),
            (keyless_empty, ReadingsSnapshot::default()),
            (logged_out_empty, ReadingsSnapshot::default()),
            (polled_unscheduled, polled_readings),
        ],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, &readings);
    for (idx, what) in [
        (0, "disabled oauth with nothing cached"),
        (1, "keyless provider with nothing cached"),
        (2, "logged-out oauth with nothing cached"),
        (
            3,
            "polled oauth with a reading and no refresh scheduled yet",
        ),
    ] {
        let line = render_overview_row(&app, idx, &widths, false, true, &readings);
        assert!(
            mark_spans(&line).is_empty(),
            "a {what} row carries no `⋯`: {}",
            line_text(&line)
        );
    }
    // Fixture control: the polled row really draws its reading, so its missing
    // mark is the work-list rule and not an empty row.
    let polled_line = render_overview_row(&app, 3, &widths, false, true, &readings);
    assert!(
        line_text(&polled_line).contains("10%"),
        "control: the polled row draws its 5h reading: {}",
        line_text(&polled_line)
    );
}

/// "Draws a reading" means what the row draws at this width: a pane too narrow
/// for the 7d column hides a 7d-only reading, so that row is dashes and keeps a
/// blank slot; the same row at full width draws its 7d bar and carries the mark.
#[test]
fn a_narrow_pane_hiding_the_7d_column_leaves_a_7d_only_row_unmarked() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let (logged_out, _) = profile("s7", 95.0, 10.0, 3600);
    let seven_only = UsageInfo {
        seven_day: Some(UsageWindow {
            utilization: 30.0,
            resets_at: Some(reset_in(3 * 86_400)),
        }),
        ..UsageInfo::default()
    };
    let (config, readings) = config_with(
        vec![(
            logged_out,
            snap_with_usage(ReadingsSnapshot::default(), "s7", seven_only),
        )],
        None,
        vec![],
    );
    let app = App::new(config);

    let narrow = OverviewWidths::new(50, &app, &readings);
    assert_eq!(
        narrow.seven_day, 0,
        "fixture control: 50 cells drop the 7d column"
    );
    let narrow_line = render_overview_row(&app, 0, &narrow, false, true, &readings);
    assert!(
        mark_spans(&narrow_line).is_empty(),
        "a row whose only reading sits in a hidden column draws dashes, so no `⋯`: {}",
        line_text(&narrow_line)
    );

    let wide = OverviewWidths::new(110, &app, &readings);
    assert!(
        wide.seven_day > 0,
        "fixture control: 110 cells keep the 7d column"
    );
    let wide_line = render_overview_row(&app, 0, &wide, false, true, &readings);
    assert_eq!(
        mark_spans(&wide_line).len(),
        1,
        "control: the same row drawing its 7d bar carries the mark: {}",
        line_text(&wide_line)
    );

    // Every 7d tier the shrink loop can pick, the narrowest (`%` only) and the
    // bar without its reset included: whenever the column draws, its figure is
    // on the row and the mark is beside it; whenever it is gone, no mark.
    let mut tiers_seen = std::collections::BTreeSet::new();
    for width in 40..=120u16 {
        let widths = OverviewWidths::new(width, &app, &readings);
        let line = render_overview_row(&app, 0, &widths, false, true, &readings);
        let marks = mark_spans(&line).len();
        if widths.seven_day > 0 {
            tiers_seen.insert(widths.seven_day);
            assert!(
                line_text(&line).contains("30%"),
                "fixture control at {width} cells: the 7d column draws its figure: {}",
                line_text(&line)
            );
            assert_eq!(marks, 1, "{width} cells draw the 7d figure, so one `⋯`");
        } else {
            assert_eq!(marks, 0, "{width} cells hide the 7d column, so no `⋯`");
        }
    }
    assert!(
        tiers_seen.len() >= 2,
        "fixture control: the sweep crosses a narrow 7d tier and the full one: {tiers_seen:?}"
    );
}

/// A row's snapshot entry is not a drawn reading: an unpolled provider account
/// whose cached stats hold rows but no 5h/7d bars draws dashes (no `⋯`), and an
/// unpolled DeepSeek account whose stats hold no balance row draws `—` in its
/// balance cell (no `⋯`). Each sits beside its drawing twin as the control.
#[test]
fn an_unpolled_row_with_stats_but_nothing_drawn_gets_no_mark() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let (keyless, mut keyless_readings) = keyless_provider_profile();
    if let Some(stats) = keyless_readings.third_party.get_mut("tp") {
        stats.bars.clear();
        stats.rows.push(crate::providers::StatRow {
            label: "credits".into(),
            value: "12.00 USD".into(),
            kind: crate::providers::StatRowKind::Body,
            raw: None,
        });
    }
    let (mut ds_empty, ds_empty_readings) = deepseek_profile("d0", &[]);
    ds_empty.disabled = true;
    let (mut ds_balance, ds_balance_readings) = deepseek_profile("d1", &["1.71 USD"]);
    ds_balance.disabled = true;
    let (config, readings) = config_with(
        vec![
            (keyless, keyless_readings),
            (ds_empty, ds_empty_readings),
            (ds_balance, ds_balance_readings),
        ],
        None,
        vec![],
    );
    assert!(
        readings
            .third_party
            .get("tp")
            .is_some_and(|s| s.bars.is_empty()),
        "fixture control: the keyless row holds stats with no bars"
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, &readings);
    let row = |idx: usize| render_overview_row(&app, idx, &widths, false, true, &readings);

    for (idx, what) in [
        (0, "keyless provider with stats and no bars"),
        (1, "disabled deepseek with no balance row"),
    ] {
        let line = row(idx);
        assert!(
            mark_spans(&line).is_empty(),
            "a {what} draws no figure, so no `⋯`: {}",
            line_text(&line)
        );
    }
    assert_eq!(
        mark_spans(&row(2)).len(),
        1,
        "control: a disabled deepseek drawing its balance carries the mark: {}",
        line_text(&row(2))
    );
}

/// The third-party leg admits only a profile its entry constructor builds an
/// entry for: a plain api key with no endpoint and no provider is on neither
/// fetch leg, so a cached OAuth reading on its row (left from a login it no
/// longer holds) is marked. A key alone is not the leg's membership rule.
#[test]
fn a_key_with_no_endpoint_is_unpolled_and_its_cached_reading_is_marked() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let (mut keyed, keyed_readings) = profile("k", 95.0, 10.0, 3600);
    keyed.api_key = Some("sk-fixture".into());
    let (mut polled, polled_readings) = profile("p", 95.0, 10.0, 3600);
    polled.credentials = Some(oauth_creds());
    let (config, readings) = config_with(
        vec![(keyed, keyed_readings), (polled, polled_readings)],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, &readings);
    let keyed_line = render_overview_row(&app, 0, &widths, false, true, &readings);
    assert_eq!(
        mark_spans(&keyed_line).len(),
        1,
        "a key with no endpoint fetches nothing, so its drawn reading is marked: {}",
        line_text(&keyed_line)
    );
    let polled_line = render_overview_row(&app, 1, &widths, false, true, &readings);
    assert!(
        mark_spans(&polled_line).is_empty(),
        "control: a logged-in sibling is polled: {}",
        line_text(&polled_line)
    );
}

// ── stale (past-reset) window fade ────────────────────────────────────────

/// Locates the 5h bracketed bar's `[`, fill, and `%` spans by content — the
/// row's only `[` (the 7d column has no synthesized window in the `profile()`
/// fixture, so it renders a bare `—`).
fn bar_fade_spans(line: &Line<'static>) -> (Style, Style, Style) {
    let idx = line
        .spans
        .iter()
        .position(|s| s.content == "[")
        .expect("the 5h bracket renders");
    (
        line.spans[idx].style,
        line.spans[idx + 1].style,
        line.spans[idx + 3].style,
    )
}

/// A past-reset 5h window is a frozen pre-reset reading: its bar fill and `%`
/// fade to `theme::faint()`. A sibling at the SAME utilization with a future
/// reset proves the difference is staleness, not the percentage — a mutation
/// dropping the fade entirely would still pass a same-value comparison, so the
/// control keeps a real (non-faint) util color to red against.
#[test]
fn stale_window_fades_bar_fill_and_percent() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (stale, stale_readings) = profile("a", 95.0, 73.0, -60); // reset 60s in the past
    let (live, live_readings) = profile("b", 95.0, 73.0, 3600); // reset in 1h
    let (config, readings) = config_with(
        vec![(stale, stale_readings), (live, live_readings)],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, &readings);

    let stale_line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let live_line = render_overview_row(&app, 1, &widths, false, true, &readings);
    let (stale_bracket, stale_fill, stale_pct) = bar_fade_spans(&stale_line);
    let (live_bracket, live_fill, live_pct) = bar_fade_spans(&live_line);

    assert_ne!(
        live_fill.fg,
        theme::faint().fg,
        "control: a live window's fill keeps its real util color"
    );
    assert_ne!(
        live_pct.fg,
        theme::faint().fg,
        "control: a live window's % keeps its real util color"
    );
    assert_eq!(
        stale_fill.fg,
        theme::faint().fg,
        "a past-reset window's fill fades"
    );
    assert_eq!(
        stale_pct.fg,
        theme::faint().fg,
        "a past-reset window's % fades"
    );
    assert_eq!(
        stale_bracket.fg, live_bracket.fg,
        "brackets stay dim regardless of staleness"
    );
}

/// Disabled always wins: `flatten` runs AFTER the stale fade and overrides
/// every span to `theme::dim()`, so a disabled + past-reset row must not show
/// a half-faint/half-dim bar — every span flattens to the same dim hue.
#[test]
fn disabled_and_past_reset_row_stays_fully_dim() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut a, a_readings) = profile("a", 95.0, 73.0, -60); // disabled + past-reset
    a.disabled = true;
    let (config, readings) = config_with(vec![(a, a_readings)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, &readings);

    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let non_dim: Vec<String> = line
        .spans
        .iter()
        .filter(|s| !s.content.trim().is_empty())
        .filter(|s| s.style.fg.is_some() && s.style.fg != theme::dim().fg)
        .map(|s| s.content.trim().to_string())
        .collect();
    // The not-current mark is the one span a disabled row keeps in color: it is
    // what says the dim figures are a cached reading.
    assert_eq!(
        non_dim,
        vec![NOT_CURRENT_MARK.to_string()],
        "a disabled row's bar must flatten fully to dim, not stay half-faint"
    );
}

// ── fallback chain panel: auto-sizing + row trailers ─────────────────────

/// Content that fits gets exactly its own height (rows + 2 border), leaving the
/// rest to the accounts table.
#[test]
fn chain_panel_height_fits_its_content() {
    assert_eq!(chain_panel_height(6, 20), 8, "6 rows + 2 border");
}

/// A long chain is capped so the accounts table keeps its `ACCOUNTS_MIN` rows —
/// accounts wins the vertical budget.
#[test]
fn chain_panel_height_caps_so_accounts_keeps_minimum() {
    assert_eq!(chain_panel_height(30, 20), 20 - ACCOUNTS_MIN);
}

/// A terminal too short for both floors the chain at 3 and never panics on the
/// clamp (max_chain saturates to 0 below the accounts minimum).
#[test]
fn chain_panel_height_floors_at_three_without_panicking() {
    assert_eq!(chain_panel_height(5, 6), 3);
    assert_eq!(chain_panel_height(0, 0), 3);
}

/// The projected switch target carries the compact `↲ ~eta` hint on its OWN row
/// (not a trailing caption), parked at the shared trailer column just past the
/// content — NOT flung out to the panel's right edge.
#[test]
fn chain_row_switch_hint_rides_the_target_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let app = App::new(config);
    let cfg = app.config();
    let row = chain_row(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        ChainRowCtx {
            index: 0,
            last: 0,
            name_w: 8,
            gauge_w: GAUGE_W,
            reading_w: 3,
            thr_w: 3,
            figure_inline: true,
            reason: None,
            switch_eta: Some(7200),
            pct: Some(10.0),
        },
    );
    let base = row.base_width();
    let line = row.into_line(base + TRAILER_GAP, 60);
    let text = line_text(&line);
    assert!(text.contains("↲ ~"), "target row carries the hint: {text}");
    let hint_w = Span::raw(format!("↲ ~{}", humanize_duration(7200))).width();
    assert_eq!(
        line.width(),
        base + TRAILER_GAP + hint_w,
        "the hint sits at the trailer column, not the panel edge: {text}",
    );
    assert!(
        line.width() < 60,
        "a 60-wide panel must leave slack past the hint: {text}",
    );
}

/// Every projected hop carries the same `↲` glyph, the preferred (home) member
/// included — the destination never changes the marker, and neither the old
/// `↩` nor a `⌂` exists on this surface.
#[test]
fn chain_row_renders_every_projected_hop_with_the_return_glyph() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut home, home_readings) = profile("home", 95.0, 10.0, 3600);
    home.preferred = true;
    let (plain, plain_readings) = profile("plain", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(
        vec![(home, home_readings), (plain, plain_readings)],
        Some("plain"),
        vec!["home", "plain"],
    );
    let app = App::new(config);
    let cfg = app.config();

    let hint = |name: &str| {
        let row = chain_row(
            &cfg,
            &crate::profile::ProfileName::from(name),
            ChainRowCtx {
                index: 0,
                last: 0,
                name_w: 8,
                gauge_w: GAUGE_W,
                reading_w: 3,
                thr_w: 3,
                figure_inline: true,
                reason: None,
                switch_eta: Some(7200),
                pct: Some(10.0),
            },
        );
        let base = row.base_width();
        line_text(&row.into_line(base + TRAILER_GAP, 60))
    };

    let home_hint = hint("home");
    assert!(
        home_hint.contains('↲') && !home_hint.contains('↩') && !home_hint.contains('⌂'),
        "a switch onto the preferred member keeps the plain return glyph: {home_hint}",
    );
    let plain_hint = hint("plain");
    assert!(
        plain_hint.contains('↲') && !plain_hint.contains('↩') && !plain_hint.contains('⌂'),
        "a switch onto a non-preferred member keeps the plain glyph: {plain_hint}",
    );
}

/// Every trailer in the panel lands in ONE column, and that column tracks the
/// widest row's content rather than the panel width. The regression this pins:
/// padding each row out to `width` stranded the markers at the far right edge of
/// a wide panel, cells away from the data they mark.
#[test]
fn fallback_panel_parks_trailers_next_to_the_content() {
    let _home = crate::testutil::HomeSandbox::new();
    // `ghost` sits in the chain with no profile behind it, so its row renders
    // the short `missing` arm. Leading with it proves the column is measured off
    // the WIDEST row rather than whichever one happens to come first.
    let (short, short_readings) = profile("ab", 100.0, 10.0, 3600);
    let (long, long_readings) = profile("a-much-longer-name", 100.0, 10.0, 3600);
    let (mut config, readings) = config_with(
        vec![(short, short_readings), (long, long_readings)],
        Some("ab"),
        vec!["ghost", "ab", "a-much-longer-name"],
    );
    config.state.auth_broken.push("ab".into());
    let app = App::new(config);

    let wide = 120;
    let lines = fallback_flow_lines(&app, wide, &readings).0;
    let marked = lines
        .iter()
        .find(|l| line_text(l).contains('×'))
        .expect("the auth-broken member shows its marker");
    assert!(
        marked.width() < wide / 2,
        "the marker parks by the content, not the panel edge: {:?} in a {wide}-wide panel",
        line_text(marked),
    );
    // The marked row carries the SHORTER name, so its marker can only sit past
    // its own content if the column came from the longer row.
    let unmarked = lines
        .iter()
        .find(|l| line_text(l).contains("a-much-longer-name"))
        .expect("the longer member renders");
    let marker_w = reason_marker(&BlockedReason::AuthBroken).width();
    assert_eq!(
        marked.width(),
        unmarked.width() + TRAILER_GAP + marker_w,
        "the trailer column is measured off the WIDEST row's content:\n{:?}\n{:?}",
        line_text(marked),
        line_text(unmarked),
    );
}

/// Thresholds of differing digit counts left-pad so the `%` signs stack,
/// instead of leaving a ragged edge between a `95%` row and a `100%` row.
#[test]
fn chain_rows_align_the_threshold_percent_column() {
    let _home = crate::testutil::HomeSandbox::new();
    let (ninety_five, ninety_five_readings) = profile("a", 95.0, 10.0, 3600);
    let (hundred, hundred_readings) = profile("b", 100.0, 10.0, 3600);
    let (config, readings) = config_with(
        vec![
            (ninety_five, ninety_five_readings),
            (hundred, hundred_readings),
        ],
        Some("a"),
        vec!["a", "b"],
    );
    let app = App::new(config);
    let texts: Vec<String> = fallback_flow_lines(&app, 60, &readings)
        .0
        .iter()
        .map(line_text)
        .collect();
    let a = texts.iter().find(|t| t.contains(" 95%")).expect("95% row");
    let b = texts.iter().find(|t| t.contains("100%")).expect("100% row");
    assert_eq!(
        a.find(" 95%").map(|i| i + 4),
        b.find("100%").map(|i| i + 4),
        "the two rows' `%` signs must land in the same column:\n{a}\n{b}",
    );
}

/// A row can be BOTH the projected switch target and blocked: the walk's
/// headroom pass only prefers a fresh candidate and falls through to a
/// stale-but-unexhausted one, so a
/// stale/soft-blocked member can still be `To`'s pick. With room for both, the
/// row shows the hint AND the marker rather than silently dropping the
/// imminent-switch projection.
#[test]
fn chain_row_shows_both_switch_hint_and_reason_marker_when_they_fit() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let app = App::new(config);
    let cfg = app.config();
    let row = chain_row(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        ChainRowCtx {
            index: 0,
            last: 0,
            name_w: 8,
            gauge_w: GAUGE_W,
            reading_w: 3,
            thr_w: 3,
            figure_inline: true,
            reason: Some(BlockedReason::AuthBroken),
            switch_eta: Some(7200),
            pct: Some(10.0),
        },
    );
    let col = row.base_width() + TRAILER_GAP;
    let text = line_text(&row.into_line(col, 60));
    assert!(text.contains('×'), "auth-broken shows the × marker: {text}");
    assert!(text.contains("↲ ~"), "and the switch hint: {text}");
}

/// Too narrow for the pair: the marker (the persistent block signal) survives
/// and the hint drops rather than the row overflowing or the marker vanishing.
/// Derives the width thresholds from the row's own natural content width
/// (`base_width`) instead of hand-counting cells, which is brittle against
/// gauge/figure formatting changes.
#[test]
fn chain_row_drops_switch_hint_before_reason_marker_when_narrow() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let app = App::new(config);
    let cfg = app.config();

    let build = || {
        chain_row(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            ChainRowCtx {
                index: 0,
                last: 0,
                name_w: 8,
                gauge_w: GAUGE_W,
                reading_w: 3,
                thr_w: 3,
                figure_inline: true,
                reason: Some(BlockedReason::AuthBroken),
                switch_eta: Some(7200),
                pct: Some(10.0),
            },
        )
    };
    let col = build().base_width() + TRAILER_GAP;
    let marker_w = reason_marker(&BlockedReason::AuthBroken).width();
    let hint_w = Span::raw(format!("↲ ~{}", humanize_duration(7200))).width();

    // Room for the marker alone at the trailer column, but not the hint (+1 sep)
    // beside it.
    let width = col + marker_w;
    assert!(
        width < col + hint_w + 1 + marker_w,
        "test width must sit strictly below the pair's requirement"
    );
    let text = line_text(&build().into_line(col, width));
    assert!(
        text.contains('×'),
        "marker survives at narrow width: {text}"
    );
    assert!(!text.contains('↲'), "hint drops first: {text}");
}

/// End to end: an auth-broken chain member surfaces its × marker in the overview
/// fallback panel — exercises the kick-lift read + `blocked_reason` wiring.
#[test]
fn fallback_panel_marks_a_blocked_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 95.0, 10.0, 3600);
    let (mut config, readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    config.state.auth_broken.push("a".into());
    let app = App::new(config);
    let joined = fallback_flow_lines(&app, 60, &readings)
        .0
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains('×'),
        "blocked member shows × in the panel:\n{joined}"
    );
}

// ── live-session `active` column ─────────────────────────────────────────────

/// One live session's registry row. `start_profile` doubles as the account it is
/// on: a session that never swapped runs where it launched, and the swap
/// attribution itself is pinned in `live_sessions.rs`'s own tests.
fn live_row(
    session_id: &str,
    member: &str,
    follows_chain: bool,
) -> crate::live_sessions::LiveSession {
    crate::live_sessions::LiveSession {
        follows_chain,
        ..crate::testutil::live_row(session_id, member)
    }
}

/// The cell sitting under the `live` header on `row`, padding included, so the
/// pin is an exact value AND proves the cell is aligned under its own header.
fn live_cell_text(widths: &OverviewWidths, row: &Line<'static>) -> String {
    let header = line_text(&overview_header(widths, false));
    let col = header
        .find("live")
        .expect("the accounts table carries a `live` header");
    line_text(row).chars().skip(col).take(widths.live).collect()
}

/// The column answers "how many `clauth start` sessions are on this account",
/// with `⇄` marking that at least one of them can be moved by the chain. It is
/// DISTINCT from the leading `●`, which marks the one profile a bare `claude`
/// authenticates as — an account can carry either, both, or neither. That split
/// is why the header reads `live`: `active` is the `●` sense app-wide.
#[test]
fn the_live_column_counts_live_sessions_and_marks_chain_followers() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![
            profile("main", 95.0, 10.0, 3600),
            profile("spare", 95.0, 20.0, 3600),
        ],
        Some("main"),
        vec![],
    );
    let mut app = App::new(config);
    app.live_sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "main", true),
        live_row("4242-1", "main", false),
        live_row("4242-2", "spare", false),
    ]);

    let widths = OverviewWidths::new(160, &app, &readings);
    let main = render_overview_row(&app, 0, &widths, false, false, &readings);
    let spare = render_overview_row(&app, 1, &widths, false, false, &readings);

    assert_eq!(
        live_cell_text(&widths, &main),
        "2  ⇄",
        "two sessions, one of them steerable"
    );
    assert_eq!(
        live_cell_text(&widths, &spare),
        "1   ",
        "a pinned session still holds the account and burns its window, but no `⇄`"
    );
}

/// `humanize_duration` goes 7 chars wide once the 7d reset lands in the 10h–23h
/// band with double-digit minutes (`10h 20m`), and the 26-cell 7d tier was sized
/// for the 6-char ceiling (`6d 23h`). The overflow leaks past the column's right
/// edge and shoves every column after it — including `live` — one cell right, so
/// the `live` header no longer lines up with its cell. Pinning the cell under the
/// header catches both the overflow and any future change to the budget.
#[test]
fn live_cell_stays_under_header_when_7d_reset_is_two_digit_hours() {
    let _home = crate::testutil::HomeSandbox::new();
    let (p, mut p_readings) = profile("main", 95.0, 10.0, 3600);
    if let Some(ref mut usage) = p_readings.usage.get_mut("main") {
        // 37200s = 10h 20m → `humanize_duration` emits the 7-char form.
        usage.seven_day = Some(UsageWindow {
            utilization: 95.0,
            resets_at: Some(reset_in(37_200)),
        });
    }
    let (config, readings) = config_with(vec![(p, p_readings)], Some("main"), vec![]);
    let mut app = App::new(config);
    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "main", true)]);

    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);

    assert_eq!(
        live_cell_text(&widths, &row),
        "1  ⇄",
        "the 7d reset suffix must not push the live cell past its header"
    );
}

/// Zero renders as nothing — a zero count is hidden rather than printed, and a
/// table full of `0`s would drown the accounts that do host something.
#[test]
fn an_account_with_no_live_sessions_renders_a_blank_live_cell() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    );
    let mut app = App::new(config);
    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "other", true)]);

    let widths = OverviewWidths::new(160, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);

    assert_eq!(live_cell_text(&widths, &row), "    ");
}

/// The column is budgeted on WIDTH alone. Were it budgeted on whether anything
/// is live, the whole table would reflow the moment someone ran `clauth start`
/// and reflow back when that session exited — so an empty fleet must lay the
/// table out exactly as a busy one does.
#[test]
fn the_live_column_holds_its_place_while_nothing_is_live() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    );
    let mut app = App::new(config);

    let idle_widths = OverviewWidths::new(160, &app, &readings);
    let idle_header = line_text(&overview_header(&idle_widths, false));
    let idle_row = line_text(&render_overview_row(
        &app,
        0,
        &idle_widths,
        false,
        false,
        &readings,
    ));

    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "main", true)]);
    let busy_widths = OverviewWidths::new(160, &app, &readings);
    let busy_header = line_text(&overview_header(&busy_widths, false));
    let busy_row = line_text(&render_overview_row(
        &app,
        0,
        &busy_widths,
        false,
        false,
        &readings,
    ));

    assert_eq!(idle_header, busy_header, "the header must not move");
    let col = idle_header.find("live").expect("a `live` header");
    assert_eq!(
        idle_row.chars().take(col).collect::<String>(),
        busy_row.chars().take(col).collect::<String>(),
        "no column left of `live` may shift when a session appears"
    );
}

/// A column that overflows its row is not dropped, it is CLIPPED — and the
/// clipping is invisible from a bar/reset count, because the tail ratatui throws
/// away is this column itself. So the fit gate needs its own pin: below the
/// width that pays for it the column must be absent, and above it the assembled
/// row must still fit.
#[test]
fn the_live_column_is_dropped_rather_than_clipped_when_it_does_not_fit() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    );
    let mut app = App::new(config);
    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "main", true)]);

    for width in 34u16..=200 {
        let widths = OverviewWidths::new(width, &app, &readings);
        let header = line_text(&overview_header(&widths, false));
        if widths.live == 0 {
            assert!(
                !header.contains("live"),
                "no column means no header at {width} cols: {header:?}"
            );
            continue;
        }
        let row = line_text(&render_overview_row(
            &app, 0, &widths, false, false, &readings,
        ));
        assert!(
            header.chars().count() <= width as usize,
            "the header overflows at {width} cols ({} cells): {header:?}",
            header.chars().count()
        );
        assert!(
            row.chars().count() <= width as usize,
            "the row overflows at {width} cols ({} cells): {row:?}",
            row.chars().count()
        );
    }
}

/// The live column must be monotone in the list-area inner width (`total`):
/// once it is present at some width it is present at every wider width, and it
/// never disappears as the terminal narrows. The tier ladders jump several
/// cells at a time, so the raw fit predicate alone would blink the column on
/// and off while resizing.
///
/// This inner width is 4 cells narrower than the terminal width the render
/// smoke test sweeps: the accounts panel takes 2 border cells + 2 horizontal
/// padding cells. Floors here therefore read 4 lower than that test's.
/// Swept at the stock label budget and at two-decimal readings, which widen
/// every usage tier.
#[test]
fn live_column_width_is_monotone_in_inner_width() {
    let decimals = LabelWidths { five: 6, seven: 6 };
    for labels in [LabelWidths::default(), decimals] {
        for max_name in 8..=22 {
            let mut prev = None;
            for total in 30..=200 {
                let width = live_column_width(max_name, total, labels);
                if let Some(prev_w) = prev
                    && width != prev_w
                {
                    assert_eq!(
                        (prev_w, width),
                        (0, LIVE_W),
                        "live column must only appear (0 -> LIVE_W), never drop or \
                         flip, at name {max_name}, total {total}, labels {labels:?} \
                         ({prev_w} -> {width})",
                    );
                }
                prev = Some(width);
            }
        }
    }
}

/// The speed column's width gate sweeps like the live column's: only
/// 0 -> TPS_W transitions as the inner width grows (never a drop), the column
/// never appears without the live column, and it never appears while the 7d
/// stamp is shed. Swept at the stock label budget and at two-decimal readings,
/// which widen every usage tier.
#[test]
fn tps_column_width_is_monotone_in_inner_width() {
    let decimals = LabelWidths { five: 6, seven: 6 };
    for labels in [LabelWidths::default(), decimals] {
        let (_, seven_excess) = labels.excess();
        for max_name in 8..=22 {
            let mut prev = None;
            for total in 30..=200 {
                let width = tps_column_width(max_name, total, labels);
                if let Some(prev_w) = prev
                    && width != prev_w
                {
                    assert_eq!(
                        (prev_w, width),
                        (0, TPS_W),
                        "tps column must only appear (0 -> TPS_W), never drop or \
                         flip, at name {max_name}, total {total}, labels {labels:?} \
                         ({prev_w} -> {width})",
                    );
                }
                prev = Some(width);
                if width > 0 {
                    assert!(
                        live_column_width(max_name, total, labels) > 0,
                        "tps on implies live on, at name {max_name}, total {total}, \
                         labels {labels:?}"
                    );
                    let (_, _, _, seven_day) = overview_tiers(max_name, total, labels);
                    assert_eq!(
                        seven_day,
                        SEVEN_DAY_MAX + seven_excess,
                        "tps on implies the 7d stamp is unshed, at name {max_name}, \
                         total {total}, labels {labels:?}"
                    );
                }
            }
        }
    }
}

// ── DeepSeek balance in the 5h column ──────────────────────────────────────

/// The cell sitting under the `5h` header on `row`, padding included. Finds the
/// column from its header so the pin proves alignment, not just presence.
fn five_hour_cell_text(widths: &OverviewWidths, deepseek: bool, row: &Line<'static>) -> String {
    let header = line_text(&overview_header(widths, deepseek));
    let col = header
        .find("5h")
        .expect("the accounts table carries a `5h` header");
    line_text(row)
        .chars()
        .skip(col)
        .take(widths.five_hour)
        .collect()
}

/// A DeepSeek balance never fabricates a figure and never closes a bracket
/// around a cut amount: the whole balance keeps its brackets while they fit,
/// sheds them before cutting, and only then ends in `…`. The `[11]` and
/// `[1132.60 CN]` shapes the char-take produced are the one outcome that must
/// never ship.
#[test]
fn deepseek_balance_sheds_brackets_before_it_ever_cuts() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) =
        config_with(vec![deepseek_profile("ds", &["1132.60 CNY"])], None, vec![]);
    let app = App::new(config);
    // `OverviewWidths` takes the accounts list's INNER width (terminal − 4).
    // Below 42 cols the 5h column is the 4-cell figure-only tier, so the whole
    // 11-cell balance must cut — with `…`, never a closed bracket around a cut
    // figure (`[11]`). At 42–60 the 12-cell column holds the whole balance,
    // brackets shed before any cut; at 80 the 17-cell column keeps its brackets.
    for (terminal, expected) in [
        (34u16, "113…"),
        (41u16, "113…"),
        (42u16, "1132.60 CNY "),
        (60u16, "1132.60 CNY "),
    ] {
        let widths = OverviewWidths::new(terminal.saturating_sub(4), &app, &readings);
        let row = render_overview_row(&app, 0, &widths, false, false, &readings);
        let cell = five_hour_cell_text(&widths, true, &row);
        assert_eq!(cell, expected, "balance cell at {terminal} cols");
    }
}

/// A long email-style chain name renders whole on desktop (no `…` with free
/// cells left) — the round-1 clamp to 18 cells was the regression.
#[test]
fn a_long_chain_name_renders_whole_on_desktop() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = "abcdefghijklmnopqrstuvwxyz";
    let a = profile(name, 95.0, 10.0, 3600);
    let (config, readings) = config_with(vec![a], Some(name), vec![name]);
    let app = App::new(config);
    for w in [80usize, 160] {
        let lines = fallback_flow_lines(&app, w, &readings).0;
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let row = texts
            .iter()
            .find(|t| t.contains(name))
            .expect("the member row renders");
        assert_eq!(
            row,
            " ╶ 1 abcdefghijklmnopqrstuvwxyz  ▰▱▱▱▱▱▱▱▱▱▱▱  10% / 95%"
        );
    }
}

/// The all-spent caption's first line carries the 2-cell lead AND the chip, so
/// its tail must wrap to `width - chip - 2` — the round-1 budget only
/// subtracted the chip and let the first line overrun the pane by two cells.
#[test]
fn the_stay_caption_wraps_without_overrunning_the_first_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let (config, readings) = config_with(vec![a], Some("a"), vec!["a"]);
    let app = App::new(config);
    // Every caption line, chip included, stays within the pane: the round-1
    // budget let the first line overrun by two cells. Pinned whole-row at the
    // six terminal widths (inner = terminal − 4 chrome cells).
    for (w, caption_lines) in [
        (
            23usize,
            &[
                "  [ stay ] on the",
                "  active",
                "  account when",
                "  all spent",
            ][..],
        ),
        (
            24usize,
            &["  [ stay ] on the active", "  account when", "  all spent"][..],
        ),
        (
            30usize,
            &["  [ stay ] on the active", "  account when all", "  spent"][..],
        ),
        (
            41usize,
            &["  [ stay ] on the active account when all", "  spent"][..],
        ),
    ] {
        let lines = fallback_flow_lines(&app, w, &readings).0;
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let start = texts
            .iter()
            .position(|t| t.contains("[ stay ]"))
            .unwrap_or_else(|| panic!("stay caption missing at inner {w}: {texts:?}"));
        let got: Vec<String> = texts[start..].to_vec();
        assert_eq!(got, caption_lines, "stay caption at inner {w} cells");
    }
}

/// DeepSeek accounts carry a USD balance where OAuth profiles carry a 5h
/// utilization window. When one is on the overview the column header names
/// both roles so the balance cell below it does not read as a mislabeled `%`.
#[test]
fn header_reads_5h_balance_when_a_deepseek_profile_is_present() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![
            profile("main", 95.0, 10.0, 3600),
            deepseek_profile("ds", &["1.71 USD"]),
        ],
        Some("main"),
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let header = line_text(&overview_header(&widths, any_deepseek(&app)));
    assert!(
        header.contains("5h / balance"),
        "header should name both roles when a DeepSeek account is present: {header:?}"
    );
}

/// Without a DeepSeek account the column is a pure 5h window readout, so the
/// header keeps its original label and does not advertise a balance it has no
/// cell for.
#[test]
fn header_keeps_plain_5h_when_no_deepseek_profile_is_present() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let header = line_text(&overview_header(&widths, any_deepseek(&app)));
    assert!(
        !header.contains("balance"),
        "no DeepSeek account means no balance in the header: {header:?}"
    );
    assert!(
        header.contains("5h"),
        "the 5h label is still there: {header:?}"
    );
}

/// A DeepSeek profile's total balance renders left-aligned and dim in the 5h
/// column, replacing the bracketed bar an OAuth profile would show there.
/// Pinned by header column so it also proves the cell sits under its header.
#[test]
fn deepseek_row_shows_total_balance_in_5h_column() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![
            profile("main", 95.0, 10.0, 3600),
            deepseek_profile("ds", &["1.71 USD"]),
        ],
        Some("main"),
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 1, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        cell.starts_with("[1.71 USD"),
        "the balance is bracketed and left-aligned: {cell:?}"
    );
    assert_eq!(
        cell.chars().count(),
        widths.five_hour,
        "the cell is exactly the column width so the next column does not shift"
    );
}

/// The cache on disk outlives the label clauth writes into it. Every DeepSeek
/// account carries a `third_party_cache.json` an older binary wrote, `profile.rs`
/// seeds it into `third_party_usage` on every start, and two populations never
/// get a rewrite at all: a disabled profile is dropped by
/// `collect_third_party_entries`, and a failing fetch never reaches the arm that
/// writes. A cell keyed on the current label alone blanks those accounts
/// permanently, which is why this reads the CAPTURED legacy bytes through the
/// production reader rather than a row built in Rust.
#[test]
fn a_deepseek_cache_written_before_the_rename_still_shows_its_balance() {
    let _home = crate::testutil::HomeSandbox::new();
    let stats: crate::providers::ThirdPartyStats =
        serde_json::from_str(crate::testutil::THIRD_PARTY_CACHE_BYTES)
            .expect("the captured legacy cache parses");
    let (ds, mut ds_readings) = deepseek_profile("ds", &[]);
    ds_readings.third_party.insert("ds".to_string(), stats);
    let (config, readings) = config_with(vec![(ds, ds_readings)], Some("ds"), vec![]);
    let app = App::new(config);

    let widths = OverviewWidths::new(120, &app, &readings);
    assert_eq!(
        widths.deepseek_amount_w, 5,
        "the legacy row still sizes the amount column (from `31.45`), not 0",
    );
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        cell.starts_with("[31.45 CNY"),
        "a cache written before the rename still renders its balance: {cell:?}"
    );
}

/// A DeepSeek profile whose balance fetch has not landed (empty rows) renders
/// the same no-data dash an OAuth profile with no window gets, so the column
/// reads as "no data yet" rather than a blank cell.
#[test]
fn deepseek_row_without_balance_shows_no_data_dash() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(vec![deepseek_profile("ds", &[])], Some("ds"), vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert_eq!(
        cell.trim(),
        "—",
        "no cached balance renders the no-data dash"
    );
}

/// Currencies align across DeepSeek rows: amounts left-pad to the widest so
/// every currency starts at the same column, with exactly one space after the
/// longest amount. Without alignment a short amount would butt its currency
/// against the column's left edge while a long one trails it.
#[test]
fn deepseek_balance_currencies_align_across_rows() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![
            deepseek_profile("short", &["1.71 USD"]),
            deepseek_profile("long", &["197.50 CNY"]),
        ],
        Some("short"),
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let short_row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let long_row = render_overview_row(&app, 1, &widths, false, false, &readings);
    let short_cell = five_hour_cell_text(&widths, true, &short_row);
    let long_cell = five_hour_cell_text(&widths, true, &long_row);

    let usd = short_cell.find("USD").expect("USD currency present");
    let cny = long_cell.find("CNY").expect("CNY currency present");
    assert_eq!(
        usd, cny,
        "currencies start at the same column:\n  {short_cell:?}\n  {long_cell:?}"
    );
    // The longest amount ("197.50") has exactly one space before its currency.
    let before_cny = long_cell.chars().nth(cny.saturating_sub(1)).unwrap_or('!');
    assert_eq!(
        before_cny, ' ',
        "the widest amount gets exactly one space gap: {long_cell:?}"
    );
}

/// A DeepSeek profile with multiple currencies above 0 shows both in the 5h
/// column, comma-joined and sorted by amount descending (highest first).
#[test]
fn deepseek_multi_currency_shows_all_above_zero() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![deepseek_profile("ds", &["1.71 USD", "100.00 CNY"])],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        cell.contains("USD") && cell.contains("CNY"),
        "both currencies render: {cell:?}"
    );
    // Higher amount first: 100.00 > 1.71, so CNY comes before USD.
    let cny = cell.find("CNY").expect("CNY present");
    let usd = cell.find("USD").expect("USD present");
    assert!(
        cny < usd,
        "higher balance (100.00 CNY) renders before 1.71 USD: {cell:?}"
    );
}

/// A DeepSeek balance right-aligns each amount to the widest amount (pad on the
/// LEFT), so the currency follows exactly one space after the widest amount and
/// a shorter amount is left-padded: `[  1.71 USD]` over `[197.50 CNY]`, never
/// `[1.71   USD]`.
#[test]
fn deepseek_balance_amounts_right_align_to_the_widest() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![
            deepseek_profile("short", &["1.71 USD"]),
            deepseek_profile("long", &["197.50 CNY"]),
        ],
        Some("short"),
        vec![],
    );
    let app = App::new(config);
    for terminal in [80u16, 120u16] {
        let widths = OverviewWidths::new(terminal.saturating_sub(4), &app, &readings);
        let short_row = render_overview_row(&app, 0, &widths, false, false, &readings);
        let long_row = render_overview_row(&app, 1, &widths, false, false, &readings);
        let short_cell = five_hour_cell_text(&widths, true, &short_row);
        let long_cell = five_hour_cell_text(&widths, true, &long_row);
        assert_eq!(
            short_cell.trim_end(),
            "[  1.71 USD]",
            "the short amount left-pads at {terminal} cols: {short_cell:?}"
        );
        assert_eq!(
            long_cell.trim_end(),
            "[197.50 CNY]",
            "the widest amount anchors the column at {terminal} cols: {long_cell:?}"
        );
    }
}

/// When only one currency is above 0, only that one shows — the zero-balance
/// currency is dropped.
#[test]
fn deepseek_multi_currency_only_shows_above_zero() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![deepseek_profile("ds", &["0.00 USD", "100.00 CNY"])],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        cell.contains("CNY") && !cell.contains("USD"),
        "only the above-zero balance renders: {cell:?}"
    );
}

/// The two-wallet ruling (owner 2026-08-28) on the overview column: a profile
/// whose captured cache carries the empty USD wallet first renders the funded
/// CNY wallet only, through the same shared selector the MCP roster ranks on.
#[test]
fn deepseek_two_wallet_renders_only_the_funded_wallet() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![deepseek_profile_from_cache(
            "tw",
            crate::testutil::CAPTURED_TWO_WALLET_DS_CACHE,
        )],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        cell.contains("498.18 CNY"),
        "the funded wallet is the rendered figure: {cell:?}",
    );
    // The currency token, not the amount string: the cell's alignment pads the
    // amount, so an asserted `0.00 USD` substring is one the renderer can never
    // produce for a dropped wallet and the absence leg would pin nothing.
    assert!(
        !cell.contains("USD"),
        "the empty wallet must not render: {cell:?}",
    );
}

/// One-wallet control for the ruling: a profile whose captured cache carries a
/// single funded wallet renders exactly as it did before the rule.
#[test]
fn deepseek_single_wallet_renders_unchanged() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![deepseek_profile_from_cache(
            "one",
            crate::testutil::CAPTURED_ONE_WALLET_DS_CACHE,
        )],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        cell.contains("3640.55 CNY"),
        "the single wallet is the rendered figure, as before: {cell:?}",
    );
}

/// When every currency is 0, the highest one still renders — an account with
/// no funds is still a real account, and a blank cell would read as no-data.
#[test]
fn deepseek_all_zero_shows_the_highest() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![deepseek_profile("ds", &["0.00 USD", "0.00 CNY"])],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    let row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let cell = five_hour_cell_text(&widths, true, &row);
    assert!(
        !cell.contains('—'),
        "a zero balance still renders, not the no-data dash: {cell:?}"
    );
    assert!(
        cell.starts_with('['),
        "still rendered as a bracketed balance: {cell:?}"
    );
}

/// `deepseek_amount_w` accounts for all totals across all currencies, so the
/// widest amount from any currency sets the padding for every profile. The
/// first currency in each cell starts at the same column across profiles.
#[test]
fn deepseek_amount_w_spans_all_currencies() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(
        vec![
            deepseek_profile("multi", &["1.71 USD", "197.50 CNY"]),
            deepseek_profile("single", &["3.14 USD"]),
        ],
        None,
        vec![],
    );
    let app = App::new(config);
    let widths = OverviewWidths::new(120, &app, &readings);
    assert_eq!(
        widths.deepseek_amount_w, 6,
        "amount_w must be 6 (from '197.50'), not capped at 4 or 5"
    );

    let multi_row = render_overview_row(&app, 0, &widths, false, false, &readings);
    let single_row = render_overview_row(&app, 1, &widths, false, false, &readings);
    let multi_cell = five_hour_cell_text(&widths, true, &multi_row);
    let single_cell = five_hour_cell_text(&widths, true, &single_row);

    // The first currency in both cells starts at the same column: position
    // 1 (bracket) + amount_w (6) + 1 (space) = 8.
    assert_eq!(
        multi_cell.find("CNY"),
        single_cell.find("USD"),
        "first currencies in both cells start at the same column:\n  {multi_cell:?}\n  {single_cell:?}"
    );
}

/// `c` cycles the Overview's harness filter, and `label_name` names the
/// harness the panel is showing for the left meta slot beside the bare
/// `ACCOUNTS` title. Nameless while every section shows, so the unfiltered
/// panel renders no left slot. Four states: all → claude → codex → shunt.
#[test]
fn the_harness_filter_cycles_and_names_itself() {
    use crate::tui::app::HarnessFilter;
    assert_eq!(HarnessFilter::default(), HarnessFilter::All);
    assert_eq!(
        HarnessFilter::All.label_name(),
        None,
        "the unfiltered label carries no harness name"
    );

    let claude = HarnessFilter::All.next();
    assert_eq!(claude, HarnessFilter::Claude);
    assert_eq!(claude.label_name(), Some("claude"));
    assert!(claude.shows_claude() && !claude.shows_codex());

    let codex = claude.next();
    assert_eq!(codex, HarnessFilter::Codex);
    assert_eq!(codex.label_name(), Some("codex"));
    assert!(codex.shows_codex() && !codex.shows_claude());

    let shunt = codex.next();
    assert_eq!(
        shunt.label_name(),
        Some("shunt"),
        "the fourth state names the shunt pool"
    );
    assert!(
        !shunt.shows_claude() && !shunt.shows_codex(),
        "the shunt filter hides the claude and codex rows"
    );
    assert_eq!(shunt.next(), HarnessFilter::All, "four states, then back");
    assert!(HarnessFilter::All.shows_claude() && HarnessFilter::All.shows_codex());
}

/// The accounts panel's top border row, which carries the panel title, its
/// left meta slot (the harness filter) and its title-right meta slot (the
/// counts).
fn accounts_title_row(app: &App, width: u16) -> String {
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 8)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), app, &app.readings_snapshot()))
        .expect("draw");
    crate::testutil::buffer_rows(term.backend().buffer())[0].clone()
}

/// The accounts panel's title row carries the harness filter: the bare
/// eyebrow unfiltered, the harness name under the filter as a left meta slot,
/// a title of its own one border cell after the eyebrow (`ACCOUNTS ─ claude`).
///
/// The panel's row is `╭─` + ` TITLE ` + `─` + ` left ` + rule + ` meta ` +
/// `─` + `╮` (the corner-adjacent dash is a border cell of the title line), so
/// the one-claude fixture's ` 1 claude ` slot (10 cells, ` meta ` plus its
/// closing dash) needs `width - 3 - 10 - 10 >= 3` unfiltered and
/// `width - 3 - 10 - 1 - left - 10 >= 3` filtered (`left` = the name plus its
/// two insets): the unfiltered slot holds at 26, and at width 25 the counts
/// slot sheds in all three rows and they read as the title, its left slot and
/// the rule alone.
#[test]
fn the_accounts_title_row_carries_the_harness_filter() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);

    assert_eq!(
        accounts_title_row(&app, 25),
        "╭─ ACCOUNTS ────────────╮",
        "unfiltered: the eyebrow and the border rule alone"
    );

    app.harness_filter = HarnessFilter::Claude;
    assert_eq!(
        accounts_title_row(&app, 25),
        "╭─ ACCOUNTS ─ claude ───╮",
        "the claude filter names the harness it shows"
    );

    app.harness_filter = HarnessFilter::Codex;
    assert_eq!(
        accounts_title_row(&app, 25),
        "╭─ ACCOUNTS ─ codex ────╮",
        "the codex filter names the harness it shows"
    );

    app.harness_filter = HarnessFilter::Codex.next();
    assert_eq!(
        accounts_title_row(&app, 25),
        "╭─ ACCOUNTS ─ shunt ────╮",
        "the shunt filter names the pool it shows"
    );
}

/// R1: the title opens with the corner-adjacent dash `╭─ ACCOUNTS` — a border
/// cell in the border token, never title styling — and the give-way arithmetic
/// counts it before the right slot keeps its ≥3 rule cells: the
/// `3 claude · 2 codex` meta holds at 46 and sheds at 45 under ` claude `, one
/// column past the pre-dash boundary.
#[test]
fn the_accounts_title_opens_with_the_corner_dash_and_the_counts_keep_their_gap() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2"]);
    let (config, _readings) = config_with(
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
            profile("cl3", 80.0, 30.0, 3_600),
        ],
        None,
        vec![],
    );
    let mut app = App::new(config);

    assert_eq!(
        accounts_title_row(&app, 25),
        "╭─ ACCOUNTS ────────────╮",
        "unfiltered: the corner-adjacent dash opens the border break"
    );

    app.harness_filter = HarnessFilter::Claude;
    assert_eq!(
        accounts_title_row(&app, 46),
        "╭─ ACCOUNTS ─ claude ─── 3 claude · 2 codex ─╮",
        "at 46 the counts keep their three rule cells behind the corner dash"
    );
    assert_eq!(
        accounts_title_row(&app, 45),
        "╭─ ACCOUNTS ─ claude ───────────────────────╮",
        "at 45 the counts shed, one column past the pre-dash boundary"
    );
}

/// Below the title line's own width ratatui clips the row from the right, the
/// left slot first; nothing sheds ahead of the clip. At 12 columns the eyebrow
/// loses its trailing inset behind the corner dash and at 13 the label fills
/// the border break, so every filter state reads the same there. From 14 the
/// slot shows as far as the row reaches: at 16 a filtered row still names its
/// harness by its first letter, and at 20 the clip takes the claude slot's
/// tail and the codex slot's closing inset. A slot that shed instead would
/// read as the unfiltered row at 16 under either filter; ` codex ` fills its
/// line whole at 21.
#[test]
fn a_title_row_narrower_than_the_eyebrow_and_its_slot_clips_the_slot() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);

    for (filter, at_16, at_20) in [
        (
            HarnessFilter::All,
            "╭─ ACCOUNTS ───╮",
            "╭─ ACCOUNTS ───────╮",
        ),
        (
            HarnessFilter::Claude,
            "╭─ ACCOUNTS ─ c╮",
            "╭─ ACCOUNTS ─ claud╮",
        ),
        (
            HarnessFilter::Codex,
            "╭─ ACCOUNTS ─ c╮",
            "╭─ ACCOUNTS ─ codex╮",
        ),
    ] {
        app.harness_filter = filter;
        assert_eq!(
            accounts_title_row(&app, 12),
            "╭─ ACCOUNTS╮",
            "{filter:?} at 12"
        );
        assert_eq!(
            accounts_title_row(&app, 13),
            "╭─ ACCOUNTS ╮",
            "{filter:?} at 13"
        );
        assert_eq!(accounts_title_row(&app, 16), at_16, "{filter:?} at 16");
        assert_eq!(accounts_title_row(&app, 20), at_20, "{filter:?} at 20");
    }
}

/// Every term of the meta slot's give-way arithmetic counts display cells,
/// never chars, each pinned at the width that holds the meta on its three rule
/// cells and one column narrower, where it sheds (`buffer_rows` reads each wide
/// glyph's continuation cell as a space). The insets count the corner-adjacent
/// dash too (3 = the two corners plus that dash):
/// - the meta: `界界` (4 cells) under the 10-cell title needs
///   `rule = width - 13 - 7 >= 3`, holding at 23; counting chars keeps it at 22.
/// - the left slot: `界界` beside a `1 x` meta needs
///   `rule = width - 13 - 1 - 6 - 6 >= 3`, holding at 29; counting chars keeps
///   it at 28.
/// - the title: `界ß` uppercases to ` 界SS ` (6 cells, 5 chars, 4 before
///   uppercasing) and needs `rule = width - 9 - 6 >= 3` beside `1 x`, holding
///   at 18; counting chars either way keeps it at 17.
#[test]
fn every_give_way_term_measures_wide_glyphs_in_display_cells() {
    let row = |title: &str, left: Option<&str>, meta: &str, width: u16| {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 3)).expect("terminal");
        term.draw(|f| {
            f.render_widget(
                section_box_meta(title, left, meta, true, true, width),
                f.area(),
            );
        })
        .expect("draw");
        crate::testutil::buffer_rows(term.backend().buffer())[0].clone()
    };

    assert_eq!(
        row("accounts", None, "界界", 23),
        "╭─ ACCOUNTS ─── 界 界  ─╮",
        "a wide meta keeps its three rule cells at 23"
    );
    assert_eq!(
        row("accounts", None, "界界", 22),
        "╭─ ACCOUNTS ─────────╮",
        "a wide meta is gone at 22"
    );
    assert_eq!(
        row("accounts", Some("界界"), "1 x", 29),
        "╭─ ACCOUNTS ─ 界 界  ─── 1 x ─╮",
        "beside a wide left slot the meta keeps its three rule cells at 29"
    );
    assert_eq!(
        row("accounts", Some("界界"), "1 x", 28),
        "╭─ ACCOUNTS ─ 界 界  ────────╮",
        "beside a wide left slot the meta is gone at 28"
    );
    assert_eq!(
        row("界ß", None, "1 x", 18),
        "╭─ 界 SS ─── 1 x ─╮",
        "under a wide, lengthening title the meta keeps its three rule cells at 18"
    );
    assert_eq!(
        row("界ß", None, "1 x", 17),
        "╭─ 界 SS ────────╮",
        "under a wide, lengthening title the meta is gone at 17"
    );
}

/// The counts slot gives way before the filter slot: its give-way arithmetic
/// counts the filter's cells, so a filtered panel needs
/// `width - 3 - 10 - 1 - left - 21 >= 3` under the `3 claude · 2 codex`
/// meta (the corner-adjacent dash, the bare gap cell, then ` meta ─`), where
/// `left` is the name plus its two insets. That holds the counts at 46 under
/// ` claude ` and 45 under ` codex `; one column narrower the counts drop
/// whole and the filter stays, since a filtered list under a bare title would
/// misread it. The 37 and 36 rows are the unfiltered shed test's widths: the
/// unfiltered panel holds its counts at 37, and a filtered one has shed them
/// at both.
#[test]
fn the_counts_slot_sheds_before_the_filter_slot() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2"]);
    let (config, _readings) = config_with(
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
            profile("cl3", 80.0, 30.0, 3_600),
        ],
        None,
        vec![],
    );
    let mut app = App::new(config);
    assert_eq!(
        app.codex_rows.len(),
        2,
        "fixture control: the roster loaded"
    );

    app.harness_filter = HarnessFilter::Claude;
    assert_eq!(
        accounts_title_row(&app, 46),
        "╭─ ACCOUNTS ─ claude ─── 3 claude · 2 codex ─╮",
        "at 46 the counts keep their three rule cells after the claude slot"
    );
    assert_eq!(
        accounts_title_row(&app, 45),
        "╭─ ACCOUNTS ─ claude ───────────────────────╮",
        "one column narrower the counts are gone and the claude slot stays"
    );
    assert_eq!(
        accounts_title_row(&app, 36),
        "╭─ ACCOUNTS ─ claude ──────────────╮",
        "at 36 the claude slot stays without the counts"
    );
    assert_eq!(
        accounts_title_row(&app, 35),
        "╭─ ACCOUNTS ─ claude ─────────────╮",
        "at 35 the claude slot stays without the counts"
    );

    app.harness_filter = HarnessFilter::Codex;
    assert_eq!(
        accounts_title_row(&app, 45),
        "╭─ ACCOUNTS ─ codex ─── 3 claude · 2 codex ─╮",
        "at 45 the counts keep their three rule cells after the codex slot"
    );
    assert_eq!(
        accounts_title_row(&app, 44),
        "╭─ ACCOUNTS ─ codex ───────────────────────╮",
        "one column narrower the counts are gone and the codex slot stays"
    );
    assert_eq!(
        accounts_title_row(&app, 36),
        "╭─ ACCOUNTS ─ codex ───────────────╮",
        "at 36 the codex slot stays without the counts"
    );
    assert_eq!(
        accounts_title_row(&app, 35),
        "╭─ ACCOUNTS ─ codex ──────────────╮",
        "at 35 the codex slot stays without the counts"
    );
}

/// The filter slot is data, styled like the title-right slot: `TEXT_DIM`,
/// never bold, never italic. The eyebrow before it keeps the first panel's
/// title treatment (`ACCENT_2`, italic, bold while focused), and the dash
/// between them is the border's own cell in the border token, so the two read
/// as a title and a note about it rather than one run of title text.
#[test]
fn the_filter_slot_is_dim_data_and_the_eyebrow_keeps_its_title_style() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (config, readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);

    for (filter, name) in [
        (HarnessFilter::Claude, "claude"),
        (HarnessFilter::Codex, "codex"),
    ] {
        app.harness_filter = filter;
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 8)).expect("terminal");
        term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
            .expect("draw");
        let buf = term.backend().buffer();
        let row = crate::testutil::buffer_rows(buf)[0].clone();
        assert!(
            row.starts_with(&format!("╭─ ACCOUNTS ─ {name} ")),
            "{filter:?}: the filter slot follows the eyebrow: {row:?}"
        );
        let eyebrow = "╭─ ".chars().count();
        let gap = "╭─ ACCOUNTS ".chars().count();
        let start = "╭─ ACCOUNTS ─ ".chars().count();

        let dash = &buf.content[gap];
        assert_eq!(dash.symbol(), "─", "{filter:?}: the gap is a border cell");
        assert_eq!(
            dash.fg,
            theme::line_strong_color(),
            "{filter:?}: the gap dash carries the focused border token"
        );
        assert!(
            !dash.modifier.intersects(Modifier::BOLD | Modifier::ITALIC),
            "{filter:?}: the gap dash takes no title or slot styling"
        );

        for x in start..start + name.len() {
            let cell = &buf.content[x];
            assert_eq!(
                cell.fg,
                theme::text_dim_color(),
                "{filter:?}: filter cell {x} is TEXT_DIM in {row:?}"
            );
            assert!(
                !cell.modifier.contains(Modifier::BOLD),
                "{filter:?}: filter cell {x} is not bold in {row:?}"
            );
            assert!(
                !cell.modifier.contains(Modifier::ITALIC),
                "{filter:?}: filter cell {x} is not italic in {row:?}"
            );
        }

        for x in eyebrow..eyebrow + "ACCOUNTS".len() {
            let cell = &buf.content[x];
            assert_eq!(
                cell.fg,
                theme::accent_2_color(),
                "{filter:?}: eyebrow cell {x} keeps the first panel's title color"
            );
            assert!(
                cell.modifier.contains(Modifier::ITALIC | Modifier::BOLD),
                "{filter:?}: eyebrow cell {x} stays italic and bold while focused"
            );
        }
    }
}

/// The meta slot counts both harnesses whatever the filter shows — the counts
/// describe the rows the table lists, never the view the filter narrows — as
/// one term per harness joined by a middot.
#[test]
fn the_accounts_meta_slot_counts_both_harnesses_whatever_the_filter_shows() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2"]);
    let (config, _readings) = config_with(
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
            profile("cl3", 80.0, 30.0, 3_600),
        ],
        None,
        vec![],
    );
    let mut app = App::new(config);
    assert_eq!(
        app.codex_rows.len(),
        2,
        "fixture control: the roster loaded"
    );

    assert_eq!(
        accounts_title_row(&app, 50),
        "╭─ ACCOUNTS ──────────────── 3 claude · 2 codex ─╮",
        "unfiltered: both rosters, no filter slot leaving a longer rule"
    );

    app.harness_filter = HarnessFilter::Claude;
    assert_eq!(
        accounts_title_row(&app, 50),
        "╭─ ACCOUNTS ─ claude ─────── 3 claude · 2 codex ─╮",
        "the claude filter leaves the counts alone"
    );

    app.harness_filter = HarnessFilter::Codex;
    assert_eq!(
        accounts_title_row(&app, 50),
        "╭─ ACCOUNTS ─ codex ──────── 3 claude · 2 codex ─╮",
        "the codex filter leaves the counts alone"
    );
}

/// A roster with no accounts drops out of the words — the slot counts what the
/// panel lists — so a claude-only config reads `2 claude` and a codex-only one
/// `2 codex` rather than naming a harness at zero.
#[test]
fn the_accounts_meta_slot_omits_a_roster_with_no_accounts() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
        ],
        None,
        vec![],
    );
    let claude_only = App::new(config);
    assert_eq!(
        accounts_title_row(&claude_only, 50),
        "╭─ ACCOUNTS ────────────────────────── 2 claude ─╮"
    );

    crate::testutil::write_codex_roster(&["cx1", "cx2"]);
    let (config, _readings) = config_with(Vec::new(), None, vec![]);
    let codex_only = App::new(config);
    assert_eq!(
        codex_only.codex_rows.len(),
        2,
        "fixture control: the roster loaded"
    );
    assert_eq!(
        accounts_title_row(&codex_only, 50),
        "╭─ ACCOUNTS ─────────────────────────── 2 codex ─╮"
    );
}

/// Both rosters empty is nothing to count: an empty meta renders the plain box,
/// the title's border rule running on to the corner.
#[test]
fn an_empty_roster_pair_renders_no_meta_slot() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(Vec::new(), None, vec![]);
    let app = App::new(config);
    assert_eq!(
        accounts_title_row(&app, 50),
        "╭─ ACCOUNTS ─────────────────────────────────────╮"
    );
}

/// An EMPTY title renders the full rule run: both the corner-adjacent dash and
/// the `title_label("")` two-space inset are skipped, so a never-titled box
/// (today only the two width-probe callers, which measure `.inner()`) cannot
/// punch a hole in the top border if it is ever rendered.
#[test]
fn an_empty_title_keeps_the_full_rule_run() {
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 3)).expect("terminal");
    term.draw(|f| f.render_widget(section_box("", true, true, f.area().width), f.area()))
        .expect("draw");
    assert_eq!(
        crate::testutil::buffer_rows(term.backend().buffer())[0],
        "╭──────────╮",
        "no dash, no hole: the border rule runs corner to corner"
    );
}

/// The slot sheds whole rather than crowding the title: at a `3 claude ·
/// 2 codex` meta under an `ACCOUNTS` title it renders while
/// `rule = width - 3 - 10 - 21 >= 3` (10 = the title and its corner-adjacent
/// dash in display cells, 21 = the bare cell plus the meta text and its
/// closing dash), so 37 holds it at the three rule cells
/// `META_RULE_MIN` asks for and 36 drops it with the rule run whole.
#[test]
fn the_accounts_meta_slot_sheds_before_the_title_loses_its_rule() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2"]);
    let (config, _readings) = config_with(
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
            profile("cl3", 80.0, 30.0, 3_600),
        ],
        None,
        vec![],
    );
    let app = App::new(config);
    assert_eq!(
        app.codex_rows.len(),
        2,
        "fixture control: the roster loaded"
    );

    assert_eq!(
        accounts_title_row(&app, 37),
        "╭─ ACCOUNTS ─── 3 claude · 2 codex ─╮",
        "at 37 the slot keeps its three rule cells"
    );
    assert_eq!(
        accounts_title_row(&app, 36),
        "╭─ ACCOUNTS ───────────────────────╮",
        "one column narrower the slot is gone and the title keeps its rule"
    );
}

/// The codex rows the Overview draws come from the codex roster plus the same
/// per-profile usage cache the codex leg writes — never from a synthesized
/// `Profile`, which would put a credential-less record into every claude path
/// that walks `config.profiles`.
#[test]
fn codex_rows_read_the_roster_and_its_own_cache() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = home.home().join(".clauth");
    crate::profile::mkdir_700(&dir).expect("mkdir .clauth");
    std::fs::write(
        dir.join("codex-profiles.toml"),
        "active_profile = \"cx2\"\nprofiles = [\"cx1\", \"cx2\"]\n",
    )
    .expect("write codex state");

    let info = crate::usage::map_codex_usage(
        r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_after_seconds":600}},"rate_limit_reset_credits":{"available_count":2}}"#,
        crate::usage::now_epoch_secs(),
    )
    .expect("maps");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("cx1"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &info,
    );
    let spent = crate::usage::map_codex_usage(
        r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000,"reset_after_seconds":600}},"rate_limit_reset_credits":{"available_count":0}}"#,
        crate::usage::now_epoch_secs(),
    )
    .expect("maps");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("cx2"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &spent,
    );

    let rows = crate::tui::app::codex_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name.as_str(), "cx1");
    assert!(!rows[0].active, "the roster's active marker is cx2's");
    assert_eq!(rows[0].plan.as_deref(), Some("plus"));
    assert_eq!(
        rows[0].five_hour.as_ref().map(|w| w.utilization),
        Some(42.0),
        "the window comes from the codex leg's own cache"
    );
    assert_eq!(
        rows[0].resets,
        Some(2),
        "the banked count rides the same cache"
    );
    assert!(rows[1].active, "cx2 holds the codex active slot");
    assert_eq!(
        rows[1].resets, None,
        "a zero count is no chip: nothing to spend"
    );
    assert!(
        rows[1].plan.is_some() && rows[1].five_hour.is_some(),
        "the spent-cache account still shows its windows"
    );
}

/// A codex cache written before `fetched_at` existed still dates its windows:
/// the row's age falls back to the cache file's mtime, so the pane reads
/// `◌ read <age> ago` over cached bars instead of `◌ not read yet` (T6 docs
/// review F9). A stamped cache keeps its own stamp.
#[test]
fn codex_rows_date_an_unstamped_cache_by_its_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2"]);
    let body = |used: u32| {
        crate::usage::map_codex_usage(
            &format!(
                r#"{{"plan_type":"plus","rate_limit":{{"primary_window":{{"used_percent":{used},"limit_window_seconds":18000,"reset_after_seconds":600}}}}}}"#
            ),
            crate::usage::now_epoch_secs(),
        )
        .expect("maps")
    };
    let unstamped = body(42);
    assert_eq!(unstamped.fetched_at, None, "the pre-T6 cache shape");
    let cx1 = crate::profile::ProfileName::from("cx1");
    crate::profile_cache::write_profile_cache(
        &cx1,
        crate::profile_cache::USAGE_CACHE_FILE,
        &unstamped,
    );
    let mtime_ms: u64 = 1_760_000_000_000;
    let path =
        crate::profile_cache::profile_cache_path(&cx1, crate::profile_cache::USAGE_CACHE_FILE)
            .expect("cache path");
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms),
    );
    let stamp_ms: u64 = 1_750_000_000_000;
    let mut stamped = body(7);
    stamped.fetched_at = Some(stamp_ms);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("cx2"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &stamped,
    );

    let rows = crate::tui::app::codex_rows();
    assert_eq!(
        rows.iter().map(|r| r.fetched_at).collect::<Vec<_>>(),
        vec![Some(mtime_ms), Some(stamp_ms)],
        "the unstamped cache reads its mtime; the stamped one its own stamp"
    );
}

/// A codex cache stamped past the clock dates its windows by the file's mtime,
/// the usage seeds' rule: a future stamp proves the clock moved, never that
/// the read is fresh (T6 residue review R4).
#[test]
fn codex_rows_date_a_future_stamped_cache_by_its_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let mut info = crate::usage::map_codex_usage(
        r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_after_seconds":600}}}"#,
        crate::usage::now_epoch_secs(),
    )
    .expect("maps");
    let day_ms: u64 = 86_400_000;
    info.fetched_at = Some(crate::usage::now_ms() + day_ms);
    let cx1 = crate::profile::ProfileName::from("cx1");
    crate::profile_cache::write_profile_cache(&cx1, crate::profile_cache::USAGE_CACHE_FILE, &info);
    let mtime_ms: u64 = 1_760_000_000_000;
    let path =
        crate::profile_cache::profile_cache_path(&cx1, crate::profile_cache::USAGE_CACHE_FILE)
            .expect("cache path");
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms),
    );

    let rows = crate::tui::app::codex_rows();
    assert_eq!(
        rows.iter().map(|r| r.fetched_at).collect::<Vec<_>>(),
        vec![Some(mtime_ms)],
        "a stamp past the clock reads the file's mtime"
    );
}

/// The plan cell's fallback: a captured-but-never-polled account still shows
/// its tier off the store's id_token `chatgpt_plan_type` claim, and the cached
/// `wham/usage` plan wins the moment a poll has answered.
#[test]
fn codex_rows_plan_falls_back_to_the_id_token_claim() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2"]);

    let id_token = crate::testutil::codex_jwt(
        r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}"#,
    );
    for name in ["cx1", "cx2"] {
        crate::testutil::write_codex_store(
            name,
            &format!(
                r#"{{"tokens":{{"id_token":"{id_token}","access_token":"a","refresh_token":"rt","account_id":"acc"}}}}"#
            ),
        );
    }

    let pro = crate::usage::map_codex_usage(
        r#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_after_seconds":600}}}"#,
        crate::usage::now_epoch_secs(),
    )
    .expect("maps");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("cx2"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &pro,
    );

    let rows = crate::tui::app::codex_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].plan.as_deref(),
        Some("plus"),
        "no cache: the id_token claim stands in"
    );
    assert_eq!(
        rows[1].plan.as_deref(),
        Some("pro"),
        "the cached plan wins over the id_token claim"
    );
}

/// A codex row's usage cells take the claude row's columns: the 5h value sits
/// under the `5h` header (the same lead-in as the claude bar), the 7d value
/// under `7d`, and when the width drops the 7d column the codex row renders no
/// 7d cell at all, so nothing lands under `live` for a row that has no live
/// sessions.
#[test]
fn a_codex_rows_usage_cells_sit_under_their_headers() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = CodexRow {
        name: crate::profile::ProfileName::from("cx1"),
        active: false,
        broken: false,
        plan: Some("pro".to_string()),
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        seven_day: None,
        resets: None,
        fetched_at: None,
    };
    let (config, readings) = config_with(vec![], None, vec![]);
    let app = App::new(config);

    let wide = OverviewWidths::new(80, &app, &readings);
    assert!(wide.seven_day > 0, "80 columns keep the 7d column");
    let line = render_codex_row(&row, &wide, false);
    assert_eq!(
        five_hour_cell_text(&wide, false, &line),
        fixed("42%", wide.five_hour),
        "the 5h value sits under the 5h header, left-aligned like the claude bar"
    );
    assert_eq!(
        seven_day_cell_text(&wide, &line),
        fixed("—", wide.seven_day),
        "a missing window is a dash under its own header"
    );

    let narrow = OverviewWidths::new(56, &app, &readings);
    assert_eq!(narrow.seven_day, 0, "56 columns drop the 7d column");
    let line = render_codex_row(&row, &narrow, false);
    assert_eq!(
        five_hour_cell_text(&narrow, false, &line),
        fixed("42%", narrow.five_hour)
    );
    assert_eq!(
        live_cell_text(&narrow, &line).trim_end(),
        "",
        "no 7d cell is rendered where the column is gone, so nothing sits under live"
    );
}

/// The banked-reset chip renders only while a reset is available, trailing the
/// usage cells: `↺ N` names what `clauth limit-reset` would spend.
#[test]
fn a_codex_row_shows_the_reset_chip_only_while_one_is_available() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = |resets: Option<i64>| CodexRow {
        name: crate::profile::ProfileName::from("cx1"),
        active: false,
        broken: false,
        plan: Some("pro".to_string()),
        five_hour: None,
        seven_day: None,
        resets,
        fetched_at: None,
    };
    let (config, readings) = config_with(vec![], None, vec![]);
    let mut app = App::new(config);
    // The chip renders only while the widths charge its slot, which needs a
    // visible codex row carrying a reset.
    app.codex_rows = vec![row(Some(2))];
    let text = |line: ratatui::text::Line<'_>| -> String {
        line.spans.iter().map(|s| s.content.clone()).collect()
    };

    let wide = OverviewWidths::new(80, &app, &readings);
    assert!(
        text(render_codex_row(&row(Some(2)), &wide, false)).contains("↺ 2"),
        "two banked resets render the chip"
    );
    assert!(
        !text(render_codex_row(&row(None), &wide, false)).contains("↺"),
        "no available reset (the constructor floors zero to None): no chip"
    );
}

/// The cell sitting under the `t/s` header on `row`, padding included; empty
/// when the column is dropped. Mirrors `five_hour_cell_text` so the pin proves
/// the cell sits under its own header, not just that a blank exists somewhere.
fn tps_column_text(widths: &OverviewWidths, row: &Line<'static>) -> String {
    if widths.tps == 0 {
        return String::new();
    }
    let header = line_text(&overview_header(widths, false));
    let col = header
        .find("t/s")
        .expect("the accounts table carries a `t/s` header");
    line_text(row).chars().skip(col).take(widths.tps).collect()
}

/// The banked-reset chip trails the t/s column the same way it trails `live`:
/// a dim `↺ N` must never land under `t/s` and read as that row's speed. Pinned
/// by column position through the header at a width with the t/s column on and
/// one with it off — the chip adds the blank t/s slot only where the column is.
#[test]
fn a_codex_reset_chip_never_sits_under_the_tps_header() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = CodexRow {
        name: crate::profile::ProfileName::from("cx1"),
        active: false,
        broken: false,
        plan: Some("pro".to_string()),
        five_hour: None,
        seven_day: None,
        resets: Some(2),
        fetched_at: None,
    };
    let (config, readings) = config_with(vec![], None, vec![]);
    let mut app = App::new(config);
    // The chip renders only while the widths charge its slot.
    app.codex_rows = vec![row.clone()];

    // t/s column on: the chip sits one gap past the blank t/s cell.
    let wide = OverviewWidths::new(200, &app, &readings);
    assert_eq!((wide.live, wide.tps), (LIVE_W, TPS_W));
    let line = render_codex_row(&row, &wide, false);
    assert_eq!(
        tps_column_text(&wide, &line),
        " ".repeat(wide.tps),
        "the t/s cell stays blank under the t/s header"
    );
    let tps_col = line_text(&overview_header(&wide, false))
        .find("t/s")
        .expect("the accounts table carries a `t/s` header");
    let text = line_text(&line);
    let chip_col = text
        .chars()
        .position(|c| c == '↺')
        .expect("two banked resets render the chip");
    assert_eq!(
        chip_col,
        tps_col + wide.tps + wide.gap,
        "the chip starts one gap past the t/s column"
    );

    // t/s column off: the chip still trails the live slot, one gap past it.
    let narrow = OverviewWidths::new(100, &app, &readings);
    assert_eq!((narrow.live, narrow.tps), (LIVE_W, 0));
    let line = render_codex_row(&row, &narrow, false);
    let live_col = line_text(&overview_header(&narrow, false))
        .find("live")
        .expect("the accounts table carries a `live` header");
    let text = line_text(&line);
    let chip_col = text
        .chars()
        .position(|c| c == '↺')
        .expect("two banked resets render the chip");
    assert_eq!(
        chip_col,
        live_col + narrow.live + narrow.gap,
        "the chip starts one gap past the live column when t/s is dropped"
    );
}

/// The trailing `↺ N` chip charge comes off the gap first, then the name down
/// to its floor, and past that the chip sheds whole: the name column is
/// untouched at wide panes, and the row never exceeds the pane. Codex-only
/// roster, one- and two-digit counts.
#[test]
fn a_codex_reset_chip_charges_the_gap_then_the_name_then_sheds() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(vec![], None, vec![]);
    let mut app = App::new(config);
    let rows = || {
        vec![
            CodexRow {
                name: crate::profile::ProfileName::from("cx1"),
                active: false,
                broken: false,
                plan: Some("pro".to_string()),
                five_hour: None,
                seven_day: None,
                resets: Some(12),
                fetched_at: None,
            },
            CodexRow {
                name: crate::profile::ProfileName::from("cx2"),
                active: false,
                broken: false,
                plan: Some("plus".to_string()),
                five_hour: None,
                seven_day: None,
                resets: Some(2),
                fetched_at: None,
            },
        ]
    };
    app.codex_rows = rows();

    // The gap absorbs the chip charge first: at these panes the name column is
    // unchanged by a banked reset.
    for inner in [132, 137, 140] {
        let name_with = OverviewWidths::new(inner as u16, &app, &readings).name;
        for r in &mut app.codex_rows {
            r.resets = None;
        }
        let name_without = OverviewWidths::new(inner as u16, &app, &readings).name;
        assert_eq!(name_with, name_without, "no name loss at inner {inner}");
        app.codex_rows = rows();
    }

    // The base row fits at every width; wherever the chip is charged the whole
    // row fits too.
    for w in 60u16..=220 {
        let inner = usize::from(w) - 4;
        let widths = OverviewWidths::new(w - 4, &app, &readings);
        let base = super::fixed_overview_width_cols(
            widths.name,
            widths.kind,
            widths.five_hour,
            widths.seven_day,
            widths.live,
            widths.tps,
            widths.gap,
        ) + super::TIMER_SLOT;
        assert!(base <= inner, "the base row exceeds the pane at {w}");
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 20)).expect("terminal");
        term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
            .expect("draw");
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        if rows.iter().any(|r| r.contains("↺")) {
            // "↺ 12" — the widest chip in the fixture — sits one gap past the
            // columns; a charged row never exceeds the pane.
            let row_w = base + widths.gap + "↺ 12".chars().count();
            assert!(row_w <= inner, "the charged row exceeds the pane at {w}");
        }
    }

    // Below the name floor the chip sheds whole: inner 30 leaves no room even
    // with the gap at its floor and the name at `NAME_READING_FLOOR`.
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 20)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        !rows.iter().any(|r| r.contains("↺")),
        "the chip sheds whole below the name floor: {rows:?}"
    );
}

/// A quarantined codex chain renders the same broken-login `×` the claude row
/// shows; a live chain keeps the blank marker cell. The quarantine read joins
/// the once-a-second `codex_rows` snapshot, never a per-frame renderer read.
#[test]
fn a_quarantined_codex_row_renders_the_broken_marker() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = home.home().join(".clauth");
    crate::profile::mkdir_700(&dir).expect("mkdir .clauth");
    std::fs::write(
        dir.join("codex-profiles.toml"),
        "active_profile = \"cx2\"\nprofiles = [\"cx1\", \"cx2\"]\n",
    )
    .expect("write codex state");

    crate::testutil::write_codex_store(
        "cx1",
        &crate::testutil::codex_auth_body("acc.1", "refresh.cx1"),
    );
    crate::testutil::write_codex_store(
        "cx2",
        &crate::testutil::codex_auth_body("acc.2", "refresh.cx2"),
    );
    crate::codex_auth::quarantine_for_test("cx1", "reused", "refresh.cx1");

    let rows = crate::tui::app::codex_rows();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].broken, "the snapshot carries the quarantine read");
    assert!(!rows[1].broken, "no record for cx2");

    let (config, readings) = config_with(vec![], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(80, &app, &readings);
    let broken = render_codex_row(&rows[0], &widths, false);
    let live = render_codex_row(&rows[1], &widths, false);

    // The codex row carries the list rows' slots (blank 2-cell cursor prefix,
    // marker cell, gap, name), so the glyph and the name sit in the claude
    // rows' columns: the name under the header's `account`, the marker two
    // cells before it.
    let header = line_text(&overview_header(&widths, false));
    let name_col = header
        .find("account")
        .expect("the accounts table carries an `account` header");
    let broken_text: Vec<char> = line_text(&broken).chars().collect();
    let live_text: Vec<char> = line_text(&live).chars().collect();
    assert_eq!(
        broken_text[..name_col + 3].iter().collect::<String>(),
        "  × cx1",
        "a quarantined chain shows the broken glyph in the marker cell"
    );
    assert_eq!(
        live_text[..name_col + 3].iter().collect::<String>(),
        "    cx2",
        "a live chain keeps the blank marker cell"
    );
    let glyph = broken
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "×")
        .expect("the broken glyph is its own span");
    assert_eq!(
        glyph.style.fg,
        theme::danger().fg,
        "the broken glyph carries the same danger hue as the claude marker"
    );
}

/// The 7d cell text, padding included, under the `7d` header; empty when the
/// column is dropped. Mirrors `five_hour_cell_text` so the pin proves the cell
/// sits under its own header, not just that a stamp exists somewhere on the row.
fn seven_day_cell_text(widths: &OverviewWidths, row: &Line<'static>) -> String {
    if widths.seven_day == 0 {
        return String::new();
    }
    let header = line_text(&overview_header(widths, false));
    let col = header
        .find("7d")
        .expect("the accounts table carries a `7d` header");
    line_text(row)
        .chars()
        .skip(col)
        .take(widths.seven_day)
        .collect()
}

/// A 12-char account row under `reset_display = both` must never lose its 5h
/// wall-clock stamp (`· HH:MM`) while the 7d column still paints only the bare
/// `XX%`. The 7d tier at inner totals 93..101 reserved 17 cells for a bar but
/// painted 4 (the bar gate is `widths.seven_day >= 18`), so the 5h clock bonus
/// starved and the stamp dropped between 96 and 97 terminal cols with nothing
/// gained.
///
/// Sweeps TERMINAL widths 40..=170; each render runs in the list-area inner
/// width 4 cells narrower (the accounts panel's 2 border cells + 1 padding cell
/// per side, same offset the render smoke test pins). Asserts the 7d stamp map
/// stays monotone and every 5h-stamp loss is paid for by a newly appeared 7d
/// bar, then pins the 96/97 boundary in both directions.
#[test]
fn five_hour_stamp_never_lost_without_a_7d_bar_gain() {
    let _home = crate::testutil::HomeSandbox::new();
    let (p, mut p_readings) = profile("aaaaaaaaaaaa", 40.0, 60.0, 3 * 3600 + 1800);
    if let Some(ref mut usage) = p_readings.usage.get_mut("aaaaaaaaaaaa") {
        usage.seven_day = Some(UsageWindow {
            utilization: 60.0,
            resets_at: Some(reset_in(4 * 86400 + 43200)),
        });
    }
    let (mut config, readings) = config_with(vec![(p, p_readings)], None, vec![]);
    config.state.reset_display = Some(crate::profile::ResetDisplay::Both);
    let app = App::new(config);

    let mut five_stamp = Vec::with_capacity((170 - 40 + 1) as usize);
    let mut seven_stamp = Vec::with_capacity((170 - 40 + 1) as usize);
    let mut seven_bar = Vec::with_capacity((170 - 40 + 1) as usize);

    for terminal in 40u16..=170 {
        let widths = OverviewWidths::new(terminal - 4, &app, &readings);
        let row = render_overview_row(&app, 0, &widths, false, false, &readings);
        let five = five_hour_cell_text(&widths, false, &row);
        let seven = seven_day_cell_text(&widths, &row);
        five_stamp.push(five.contains('·'));
        seven_stamp.push(seven.contains('·'));
        seven_bar.push(seven.contains('['));
    }

    // The 7d stamp never blinks: once it appears at some width it stays at
    // every wider width, like the live column.
    for i in 1..seven_stamp.len() {
        assert!(
            !seven_stamp[i - 1] || seven_stamp[i],
            "7d stamp disappears at terminal width {}",
            40 + i
        );
    }

    // A width that drops the 5h stamp must newly show the 7d bar at that same
    // width, so the loss is a real trade, never a bare gutter.
    for i in 1..five_stamp.len() {
        if five_stamp[i - 1] && !five_stamp[i] {
            assert!(
                seven_bar[i] && !seven_bar[i - 1],
                "5h stamp lost with no new 7d bar at terminal width {}",
                40 + i
            );
        }
    }

    // The measured boundary, pinned in both directions: 96 and 97 cols both
    // keep the stamp. Before the fix 97 dropped it while 96 kept it.
    assert!(five_stamp[96 - 40], "5h stamp present at 96 cols");
    assert!(
        five_stamp[97 - 40],
        "5h stamp present at 97 cols (the defect width)"
    );
}

// ── peak-rate marker (▲) ─────────────────────────────────────────────────────

/// A table whose `deepseek` store key holds one model with a flat base plus a
/// `start`–`end` window: "00:00"–"24:00" covers every hour (peak whatever the
/// real clock says), "12:00"–"12:00" no hour (never peak) — the two
/// deterministic fixtures the marker tests need, time-independent by
/// construction. The store-key shape is the point: the indicator is
/// provider-bound, so the table must carry the provider's own store row.
fn windowed_table(start: &str, end: &str) -> crate::pricing::PriceTable {
    crate::pricing::PriceTable::store_key_table(
        "deepseek",
        crate::pricing::PricedModel {
            id: "deepseek-v4-pro".to_owned(),
            prices: vec![
                crate::pricing::PriceEntry {
                    input: 0.5,
                    output: 1.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    constraint: None,
                    window_only: false,
                },
                crate::pricing::PriceEntry {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    constraint: Some(crate::pricing::Constraint::TimeWindow {
                        start: start.to_owned(),
                        end: end.to_owned(),
                    }),
                    window_only: false,
                },
            ],
            effective_at: None,
        },
        "2026-01-01",
    )
}

/// A profile on the deepseek endpoint — the provider whose store rows the
/// fixture tables carry — over the shared `profile` fixture shape. The peak
/// marker is provider-bound: what the profile pins never feeds it.
fn peak_profile(name: &str) -> (Profile, ReadingsSnapshot) {
    let (mut p, p_readings) = profile(name, 40.0, 20.0, 3600);
    p.base_url = Some("https://api.deepseek.com/anthropic".into());
    p.provider = Some(crate::providers::Provider::DeepSeek);
    (p, p_readings)
}

/// An active profile on peak hours keeps its `●` — the active dot outranks
/// the peak marker, so `▲` never takes its slot.
#[test]
fn active_dot_outranks_the_peak_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (config, readings) = config_with(vec![peak_profile("a")], Some("a"), vec![]);
    let mut app = App::new(config);
    app.price_table = Some(windowed_table("00:00", "24:00"));
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let text = line_text(&line);
    assert!(text.contains('●'), "active peak row keeps ●: {text}");
    assert!(
        !text.contains('▲'),
        "the dot outranks the peak marker: {text}"
    );
    let marker = line.spans.iter().find(|s| s.content == "●").unwrap();
    assert_eq!(marker.style.fg, Some(theme::accent_2_color()));
}

/// A non-active profile on peak hours shows `▲` — the marker names the
/// surcharged rate on every row the active dot doesn't already claim.
#[test]
fn peak_marker_renders_on_inactive_rows() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (config, readings) = config_with(
        vec![peak_profile("a"), peak_profile("b")],
        Some("a"),
        vec![],
    );
    let mut app = App::new(config);
    app.price_table = Some(windowed_table("00:00", "24:00"));
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 1, &widths, false, true, &readings);
    let text = line_text(&line);
    assert!(text.contains('▲'), "inactive peak row renders ▲: {text}");
    assert!(
        !text.contains('●'),
        "no active dot on the inactive row: {text}"
    );
    let marker = line.spans.iter().find(|s| s.content == "▲").unwrap();
    assert_eq!(marker.style.fg, theme::warning().fg);
}

/// A disabled non-active profile on peak hours keeps its `▲` glyph, but the
/// `hue` closure flattens it to dim like every other marker on a disabled row.
#[test]
fn disabled_peak_row_dims_the_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut p, p_readings) = peak_profile("a");
    p.disabled = true;
    let (config, readings) = config_with(vec![(p, p_readings)], None, vec![]);
    let mut app = App::new(config);
    app.price_table = Some(windowed_table("00:00", "24:00"));
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let text = line_text(&line);
    assert!(text.contains('▲'), "disabled peak row keeps ▲: {text}");
    let marker = line.spans.iter().find(|s| s.content == "▲").unwrap();
    assert_eq!(
        marker.style.fg,
        theme::dim().fg,
        "the ▲ dims like every marker on a disabled row"
    );
}

/// A profile whose window is never active (off-peak by fixture) keeps its
/// `●` and renders no `▲` — off-peak is the resting state and stays clean.
#[test]
fn off_peak_row_keeps_the_active_dot() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    // An empty window ("12:00"–"12:00") is active at no hour.
    let (config, readings) = config_with(vec![peak_profile("a")], Some("a"), vec![]);
    let mut app = App::new(config);
    app.price_table = Some(windowed_table("12:00", "12:00"));
    let widths = OverviewWidths::new(80, &app, &readings);
    let text = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(text.contains('●'), "off-peak active row keeps ●: {text}");
    assert!(!text.contains('▲'), "no peak marker off-peak: {text}");
}

/// A usage alert (`!`) outranks the peak marker, like every other marker.
#[test]
fn bell_outranks_the_peak_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (config, readings) = config_with(vec![peak_profile("a")], Some("a"), vec![]);
    let mut app = App::new(config);
    app.price_table = Some(windowed_table("00:00", "24:00"));
    app.bell_fired.observe("a", Some(50.0), Some(60.0), true);
    let widths = OverviewWidths::new(80, &app, &readings);
    let text = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(text.contains('!'), "{text}");
    assert!(
        !text.contains('▲'),
        "bell yields to nothing but ⊖×⊘: {text}"
    );
}

/// Without a price table (still loading, or a failed fetch) no row claims
/// peak — an element with no status shows nothing.
#[test]
fn no_table_no_peak_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) = config_with(vec![peak_profile("a")], Some("a"), vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(80, &app, &readings);
    let text = line_text(&render_overview_row(
        &app, 0, &widths, false, true, &readings,
    ));
    assert!(!text.contains('▲'), "no table, no marker: {text}");
    assert!(text.contains('●'), "the active dot is untouched: {text}");
}

// ── the accounts scrollbar measures every row the panel renders ───────────────

/// The accounts panel's scrollbar column, one char per list row: the padding
/// cell right of the list (`section_box` borders + pads one cell each side, so
/// it sits at `width - 2`), from the row under the column header down to the
/// bottom border.
fn accounts_scrollbar_column(app: &App, width: u16, height: u16) -> String {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), app, &app.readings_snapshot()))
        .expect("draw");
    let buf = term.backend().buffer();
    (2..height - 1)
        .map(|y| buf.cell((width - 2, y)).expect("cell").symbol().to_string())
        .collect()
}

/// Two claude rows never overflow a 5-row list on their own; with a codex
/// section (a spacer, a header line, three rows) the same list holds seven
/// rows, and the scrollbar must say so: it measures every row pushed, never
/// the claude rows alone. Both controls render no track at all.
#[test]
fn the_accounts_scrollbar_counts_the_codex_rows() {
    let _home = crate::testutil::HomeSandbox::new();
    let claude = || {
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
        ]
    };

    let (config, _readings) = config_with(claude(), None, vec![]);
    let no_codex = App::new(config);
    assert_eq!(
        accounts_scrollbar_column(&no_codex, 80, 8),
        "     ",
        "two claude rows fit a 5-row list: no track"
    );

    crate::testutil::write_codex_roster(&["cx1", "cx2", "cx3"]);
    let (config, _readings) = config_with(claude(), None, vec![]);
    let with_codex = App::new(config);
    assert_eq!(
        with_codex.codex_rows.len(),
        3,
        "fixture control: the roster loaded"
    );

    assert_eq!(
        accounts_scrollbar_column(&with_codex, 80, 12),
        "         ",
        "seven rows fit a 9-row list: no track"
    );
    assert_eq!(
        accounts_scrollbar_column(&with_codex, 80, 8),
        "┃┃┃┊┊",
        "seven rows overflow a 5-row list: thumb 5*5/7 = 3 rows at offset 0, then track"
    );
}

#[test]
fn selecting_the_active_codex_row_keeps_its_active_name_color() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    crate::testutil::write_codex_roster(&["cx1"]);
    let (config, readings) = config_with(vec![], None, vec![]);
    let mut app = App::new(config);
    app.codex_rows[0].active = true;
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
        .expect("draw");
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);
    let x = rows[3].find("cx1").expect("active Codex row is selected") as u16;
    assert!(rows[3].contains("❯"), "selection is visible: {rows:?}");
    assert_eq!(
        buf.cell((x, 3)).expect("name cell").fg,
        theme::accent().fg.expect("accent color")
    );
}

#[test]
fn the_last_codex_row_and_thumb_follow_a_short_viewport() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2", "cx3"]);
    let (config, readings) = config_with(
        vec![
            profile("cl1", 80.0, 10.0, 3_600),
            profile("cl2", 80.0, 20.0, 3_600),
        ],
        None,
        vec![],
    );
    let mut app = App::new(config);
    assert_eq!(app.codex_rows.len(), 3);
    app.overview_cursor = 4;
    assert_eq!(
        app.current_main_item().map(|item| format!("{item:?}")),
        Some("Codex(2)".to_string())
    );
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows[6].contains("❯") && rows[6].contains("cx3"),
        "last row: {rows:?}"
    );
    assert_eq!(accounts_scrollbar_column(&app, 80, 8), "┊┊┃┃┃");
    app.harness_filter = crate::tui::app::HarnessFilter::Codex;
    app.overview_cursor = 2;
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
        .expect("codex-only draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows[5].contains("❯") && rows[5].contains("cx3"),
        "codex-only: {rows:?}"
    );
    assert_eq!(accounts_scrollbar_column(&app, 80, 8), "     ");
}

/// The accounts list's rows with the panel border and padding cut off and
/// trailing blanks trimmed, so a heading line reads as its own text.
fn accounts_list_lines(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), app, &app.readings_snapshot()))
        .expect("draw");
    crate::testutil::buffer_rows(term.backend().buffer())
        .iter()
        .map(|row| {
            row.trim_start_matches('│')
                .trim_end_matches('│')
                .trim()
                .to_string()
        })
        .collect()
}

/// The codex section's heading is the harness name alone (cloudy
/// 2026-10-14: the `— switch with clauth <name>` tail is gone).
#[test]
fn the_codex_heading_is_the_harness_name_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let app = App::new(config);
    let lines = accounts_list_lines(&app, 100, 12);
    assert!(
        lines.iter().any(|line| line == "codex"),
        "a heading line reads `codex` alone: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("switch with")),
        "no switch hint on the heading: {lines:?}"
    );
}

// ── the shunt pool section (B2; design record "Pool rows") ─────────────────

/// A pool account with every flag clear and no readings: available, unpaused.
fn pool_account(name: &str) -> crate::gateway_admin::PoolAccount {
    crate::gateway_admin::PoolAccount {
        name: name.to_string(),
        available: true,
        near_quota: false,
        cooldown_secs_remaining: None,
        needs_relogin: false,
        paused: false,
        disabled: false,
        utilization_5h: None,
        reset_5h: None,
        utilization_7d: None,
        reset_7d: None,
        utilization_7d_oi: None,
        reset_7d_oi: None,
        plan: None,
        status: None,
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
}

/// Three providers, five pool rows: `alpha` (readings, available), `beta`
/// (needs a re-login), `cx-work` (cooling down 240 s), `spare` (paused), `k1`
/// (kimi, no actions).
fn sample_pool() -> crate::tui::app::ShuntPool {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    let now = now_epoch();
    crate::tui::app::ShuntPool::Read(vec![
        PoolProvider {
            name: "claude-main".to_string(),
            auth: PoolAuth::Claude,
            accounts: vec![
                PoolAccount {
                    plan: Some("max".to_string()),
                    utilization_5h: Some(0.12),
                    reset_5h: Some(now + 3 * 3600),
                    utilization_7d: Some(0.40),
                    reset_7d: Some(now + 5 * 86_400),
                    ..pool_account("alpha")
                },
                PoolAccount {
                    available: false,
                    needs_relogin: true,
                    plan: Some("max".to_string()),
                    ..pool_account("beta")
                },
            ],
        },
        PoolProvider {
            name: "codex".to_string(),
            auth: PoolAuth::Codex,
            accounts: vec![
                PoolAccount {
                    available: false,
                    cooldown_secs_remaining: Some(240),
                    plan: Some("plus".to_string()),
                    utilization_5h: Some(0.55),
                    reset_5h: Some(now + 3600),
                    ..pool_account("cx-work")
                },
                PoolAccount {
                    available: false,
                    paused: true,
                    ..pool_account("spare")
                },
            ],
        },
        PoolProvider {
            name: "kimi".to_string(),
            auth: PoolAuth::Kimi,
            accounts: vec![pool_account("k1")],
        },
    ])
}

/// One claude account plus [`sample_pool`], the pool read in.
fn pool_app() -> App {
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    app.shunt_pool = sample_pool();
    app
}

/// Each provider in the listing gets one `shunt (<provider>)` section heading
/// after the claude (and codex) rows, its accounts under it in listing order,
/// and the title-right meta counts the pool rows.
#[test]
fn pool_rows_list_under_one_heading_per_provider() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = pool_app();
    let lines = accounts_list_lines(&app, 100, 20);
    let at = |pred: &dyn Fn(&str) -> bool, what: &str| {
        lines
            .iter()
            .position(|line| pred(line))
            .unwrap_or_else(|| panic!("{what} not drawn: {lines:?}"))
    };
    let order = [
        at(&|l| l.contains("cl1"), "the claude row"),
        at(&|l| l == "shunt (claude-main)", "the claude-main heading"),
        at(&|l| l.contains("alpha"), "alpha"),
        at(&|l| l.contains("beta"), "beta"),
        at(&|l| l == "shunt (codex)", "the codex provider heading"),
        at(&|l| l.contains("cx-work"), "cx-work"),
        at(&|l| l.contains("spare"), "spare"),
        at(&|l| l == "shunt (kimi)", "the kimi heading"),
        at(&|l| l.contains("k1"), "k1"),
    ];
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "claude rows, then each provider heading with its accounts: {lines:?}"
    );
    assert!(
        accounts_title_row(&app, 60).ends_with(" 1 claude · 5 shunt ─╮"),
        "the meta counts the five pool rows: {:?}",
        accounts_title_row(&app, 60)
    );
}

/// The foreground and modifiers of the first cell of the accounts-list line
/// whose trimmed text is exactly `heading`, drawn with the Overview cursor on
/// flat row `cursor`.
fn heading_cell(
    app: &mut App,
    cursor: usize,
    heading: &str,
) -> (ratatui::style::Color, ratatui::style::Modifier) {
    app.overview_cursor = cursor;
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), app, &app.readings_snapshot()))
        .expect("draw");
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.trim_start_matches('│').trim_end_matches('│').trim() == heading)
        .unwrap_or_else(|| panic!("heading {heading:?} not drawn: {rows:?}"));
    let x = row
        .chars()
        .position(|c| c != '│' && c != ' ')
        .expect("a heading has text");
    let cell = buf
        .cell((
            u16::try_from(x).expect("x fits"),
            u16::try_from(y).expect("y fits"),
        ))
        .expect("cell in the buffer");
    (cell.fg, cell.modifier)
}

/// The `codex` and every `shunt (<provider>)` heading render `TEXT_DIM + bold`
/// at rest and `TEXT + bold` while the cursor rests on a row under it, the
/// list section-header rule (cloudy 2026-10-15, over the mockup's plain dim);
/// only the heading
/// over the cursor lifts.
#[test]
fn a_section_heading_lifts_only_while_a_row_under_it_holds_the_cursor() {
    use crate::tui::theme;
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    crate::testutil::write_codex_roster(&["cx1"]);
    let mut app = pool_app();
    // flat rows: cl1 0 · cx1 1 · alpha 2, beta 3 (claude-main) ·
    // cx-work 4, spare 5 (codex) · k1 6 (kimi)
    assert_eq!(app.main_items().len(), 7, "the fixture's flat rows");
    let rest = (theme::text_dim_color(), Modifier::BOLD);
    let lifted = (theme::text_color(), Modifier::BOLD);
    let headings = [
        "codex",
        "shunt (claude-main)",
        "shunt (codex)",
        "shunt (kimi)",
    ];
    let cases: [(usize, Option<&str>); 6] = [
        (0, None),
        (1, Some("codex")),
        (2, Some("shunt (claude-main)")),
        (3, Some("shunt (claude-main)")),
        (4, Some("shunt (codex)")),
        (6, Some("shunt (kimi)")),
    ];
    for (cursor, lifted_heading) in cases {
        for heading in headings {
            let want = if Some(heading) == lifted_heading {
                lifted
            } else {
                rest
            };
            assert_eq!(
                heading_cell(&mut app, cursor, heading),
                want,
                "heading {heading:?} with the cursor on flat row {cursor}"
            );
        }
    }
}

/// An unread pool's one heading has no row under it, so it stays at rest,
/// `TEXT_DIM + bold`, wherever the cursor is.
#[test]
fn an_unread_pool_heading_rests_dim_and_bold() {
    use crate::tui::theme;
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    app.shunt_pool = crate::tui::app::ShuntPool::NotRead("the gateway did not answer".to_string());
    assert_eq!(
        heading_cell(&mut app, 0, "shunt — not read: the gateway did not answer"),
        (theme::text_dim_color(), Modifier::BOLD)
    );
}

/// At narrow widths, under the shunt filter, the pool band alone sizes the
/// usage columns: a `73.42%` reading still renders whole and its `%` lines up
/// with a `5%` one, where a column sized for `100%` would cut it.
#[test]
fn a_narrow_shunt_view_sizes_its_columns_for_the_pool_band() {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(vec![profile("cl1", 12.0, 30.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    app.harness_filter = crate::tui::app::HarnessFilter::Shunt;
    let now = now_epoch();
    app.shunt_pool = crate::tui::app::ShuntPool::Read(vec![PoolProvider {
        name: "p".to_string(),
        auth: PoolAuth::Claude,
        accounts: vec![
            PoolAccount {
                utilization_5h: Some(0.05),
                reset_5h: Some(now + 3 * 3600),
                utilization_7d: Some(0.05),
                reset_7d: Some(now + 5 * 86_400),
                ..pool_account("al")
            },
            PoolAccount {
                utilization_5h: Some(0.7342),
                reset_5h: Some(now + 3 * 3600),
                utilization_7d: Some(0.7342),
                reset_7d: Some(now + 5 * 86_400),
                ..pool_account("be")
            },
        ],
    }]);
    for width in [34_u16, 45] {
        // Untrimmed rows: the caret gutter keeps both rows' columns in step.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 10))
            .expect("terminal");
        term.draw(|f| draw_overview_accounts(f, f.area(), &app, &app.readings_snapshot()))
            .expect("draw");
        let lines = crate::testutil::buffer_rows(term.backend().buffer());
        let row = |name: &str| {
            lines
                .iter()
                .find(|line| {
                    line.trim_start_matches('│')
                        .trim_start_matches([' ', '❯'])
                        .starts_with(&format!("{name} "))
                })
                .unwrap_or_else(|| panic!("{name} not drawn at {width}: {lines:?}"))
                .clone()
        };
        let first_percent = |line: &str| line.chars().position(|c| c == '%');
        let (al, be) = (row("al"), row("be"));
        assert!(
            be.contains("73.42%"),
            "the wide reading renders whole at {width}: {be:?}"
        );
        assert_eq!(
            first_percent(&al),
            first_percent(&be),
            "the band's `%` signs align at {width}:\n{al:?}\n{be:?}"
        );
    }
}

/// The pool rows are a band of their own in the usage columns, like the codex
/// rows: each reading is left-padded to the band's widest, so a `5%` and a
/// `73.42%` pool reading end their `%` in the same column, in the 5h and the 7d
/// column alike, beside a claude row whose own readings stay short.
#[test]
fn pool_readings_align_their_percent_signs_within_their_band() {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(vec![profile("cl1", 12.0, 30.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    let now = now_epoch();
    app.shunt_pool = crate::tui::app::ShuntPool::Read(vec![PoolProvider {
        name: "claude-main".to_string(),
        auth: PoolAuth::Claude,
        accounts: vec![
            PoolAccount {
                utilization_5h: Some(0.05),
                reset_5h: Some(now + 3 * 3600),
                utilization_7d: Some(0.05),
                reset_7d: Some(now + 5 * 86_400),
                ..pool_account("alpha")
            },
            PoolAccount {
                utilization_5h: Some(0.7342),
                reset_5h: Some(now + 3 * 3600),
                utilization_7d: Some(0.7342),
                reset_7d: Some(now + 5 * 86_400),
                ..pool_account("beta")
            },
        ],
    }]);
    let lines = accounts_list_lines(&app, 140, 12);
    let row = |name: &str| {
        lines
            .iter()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("{name} not drawn: {lines:?}"))
            .clone()
    };
    let percents = |line: &str| -> Vec<usize> {
        line.chars()
            .enumerate()
            .filter(|(_, c)| *c == '%')
            .map(|(i, _)| i)
            .collect()
    };
    let (alpha, beta) = (row("alpha"), row("beta"));
    assert!(
        beta.contains("73.42%"),
        "the wide reading renders whole: {beta:?}"
    );
    assert_eq!(percents(&alpha).len(), 2, "two readings on {alpha:?}");
    assert_eq!(
        percents(&alpha),
        percents(&beta),
        "the band's `%` signs align:\n{alpha:?}\n{beta:?}"
    );
}

/// A pool row carries its state only through cues the claude and codex rows
/// already use: the broken-login glyph in DANGER before a name that needs a
/// re-login, a whole row dimmed while paused, a cooldown's remaining time in
/// the timer slot in WARNING; no status word anywhere, the bars for readings.
#[test]
fn pool_row_status_rides_the_existing_row_cues() {
    use crate::tui::theme;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let app = pool_app();
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &app.readings_snapshot()))
        .expect("draw");
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);

    let relogin =
        crate::testutil::needle_cells(buf, &format!("{} beta", theme::dead_credential_glyph()))
            .unwrap_or_else(|| panic!("beta carries the broken-login glyph: {rows:?}"));
    assert_eq!(
        relogin[0].fg,
        theme::danger().fg.expect("danger color"),
        "the glyph is DANGER"
    );

    let spare = crate::testutil::needle_cells(buf, "spare").expect("spare drawn");
    assert!(
        spare
            .iter()
            .all(|cell| cell.fg == theme::dim().fg.expect("dim color")),
        "a paused account's name dims like a disabled claude account"
    );

    let cx_row = rows
        .iter()
        .find(|row| row.contains("cx-work"))
        .expect("cx-work drawn");
    assert!(
        cx_row.contains(" 4m "),
        "the cooldown's 240 s reads `4m` in the timer slot: {cx_row:?}"
    );
    let cooldown = crate::testutil::needle_cells(buf, "4m").expect("cooldown drawn");
    assert!(
        cooldown
            .iter()
            .all(|cell| cell.fg == theme::warning().fg.expect("warning color")),
        "the cooldown is WARNING"
    );

    let alpha_row = rows
        .iter()
        .find(|row| row.contains("alpha"))
        .expect("alpha drawn");
    assert!(
        alpha_row.contains("12%") && alpha_row.contains("40%") && alpha_row.contains("max"),
        "alpha shows its plan and both readings as percentages: {alpha_row:?}"
    );

    for word in [
        "relogin",
        "cooling",
        "paused",
        "disabled",
        "quota",
        "available",
    ] {
        assert!(
            !rows.iter().any(|row| row.contains(word)),
            "no status word `{word}` on any row: {rows:?}"
        );
    }
}

/// A listing that cannot be read names no provider: one resting heading
/// carries the reason, no pool rows, and the meta counts no shunt term.
#[test]
fn an_unread_pool_is_one_heading_with_its_reason() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    app.shunt_pool = crate::tui::app::ShuntPool::NotRead("the gateway is not running".to_string());
    let lines = accounts_list_lines(&app, 100, 12);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("shunt"))
            .collect::<Vec<_>>(),
        vec!["shunt — not read: the gateway is not running"],
        "one heading with the reason, no provider headings: {lines:?}"
    );
    assert!(
        accounts_title_row(&app, 60).ends_with(" 1 claude ─╮"),
        "no shunt term while the pool is not read: {:?}",
        accounts_title_row(&app, 60)
    );
}

/// Wire strings on a pool row (account name, plan, provider) reach the screen
/// escaped, as the heading, menu, confirm and toasts already escape them: no
/// bidi or control symbol lands in a cell.
#[test]
fn pool_names_off_the_wire_render_escaped() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = pool_app();
    if let crate::tui::app::ShuntPool::Read(providers) = &mut app.shunt_pool {
        providers[0].name = "pro\u{202e}vider".to_string();
        providers[0].accounts[0].name = "al\u{202e}pha".to_string();
        providers[0].accounts[0].plan = Some("m\u{202e}x".to_string());
    }
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &app.readings_snapshot()))
        .expect("draw");
    let raw: Vec<String> = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .filter(|s| s.contains('\u{202e}'))
        .collect();
    assert!(
        raw.is_empty(),
        "a raw bidi symbol reached the buffer: {raw:?}"
    );
}

/// The running pool row shows the braille spinner in its marker slot.
#[test]
fn a_running_pool_row_shows_the_spinner() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = pool_app();
    app.pool_mutation_row = Some((
        crate::gateway_admin::PoolFamily::Claude,
        crate::gateway_admin::ShuntAccountName::new("alpha").expect("valid"),
    ));
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &app.readings_snapshot()))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let alpha_row = rows
        .iter()
        .find(|row| row.contains("alpha"))
        .expect("alpha drawn");
    assert!(
        alpha_row.contains("⠋"),
        "the running row shows the braille spinner: {alpha_row:?}"
    );
}

/// The spinner keys on the account's `(family, name)`, never the row index, so
/// a reorder under a running mutation cannot move it onto another account.
#[test]
fn the_spinner_follows_the_account_when_the_listing_reorders() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = pool_app();
    app.pool_mutation_row = Some((
        crate::gateway_admin::PoolFamily::Claude,
        crate::gateway_admin::ShuntAccountName::new("alpha").expect("valid"),
    ));
    if let crate::tui::app::ShuntPool::Read(providers) = &mut app.shunt_pool {
        providers[0].accounts.swap(0, 1);
    }
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &app.readings_snapshot()))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let alpha_row = rows
        .iter()
        .find(|row| row.contains("alpha"))
        .expect("alpha drawn");
    let beta_row = rows
        .iter()
        .find(|row| row.contains("beta"))
        .expect("beta drawn");
    assert!(
        alpha_row.contains("⠋"),
        "the spinner follows alpha across the reorder: {alpha_row:?}"
    );
    assert!(
        !beta_row.contains("⠋"),
        "the spinner does not land on beta (the old index): {beta_row:?}"
    );
}

/// A selected paused pool row keeps the current-row bold over its dim color,
/// the claude row's `bold_when` promotion.
#[test]
fn a_selected_paused_pool_row_keeps_the_current_row_bold() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = pool_app();
    app.overview_cursor = 4;
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &app.readings_snapshot()))
        .expect("draw");
    let spare =
        crate::testutil::needle_cells(term.backend().buffer(), "spare").expect("spare drawn");
    assert!(
        spare
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::BOLD)),
        "a selected paused row's label takes the current-row bold: {:?}",
        spare.iter().map(|cell| cell.modifier).collect::<Vec<_>>()
    );
}

/// A sub-minute cooldown reads its seconds, the arm the shared
/// `duration_label` gained when the pool's formatter and the status feed's
/// merged.
#[test]
fn a_sub_minute_cooldown_reads_seconds() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = pool_app();
    if let crate::tui::app::ShuntPool::Read(providers) = &mut app.shunt_pool {
        providers[0].accounts[0].cooldown_secs_remaining = Some(45);
    }
    let lines = accounts_list_lines(&app, 100, 20);
    assert!(
        lines.iter().any(|line| line.contains("45s")),
        "a 45 s cooldown reads `45s` in the timer slot: {lines:?}"
    );
}

/// No adopted gateway: no shunt section at all.
#[test]
fn no_adopted_gateway_draws_no_shunt_section() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    app.shunt_pool = crate::tui::app::ShuntPool::Absent;
    let lines = accounts_list_lines(&app, 100, 12);
    assert!(
        !lines.iter().any(|line| line.contains("shunt")),
        "nothing names shunt: {lines:?}"
    );
}

/// The shunt filter shows the pool rows alone; the meta still counts every
/// section, as it does under the claude and codex filters.
#[test]
fn the_shunt_filter_shows_the_pool_rows_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = pool_app();
    app.harness_filter = crate::tui::app::HarnessFilter::Codex.next();
    let lines = accounts_list_lines(&app, 100, 20);
    assert!(
        !lines.iter().any(|line| line.contains("cl1")),
        "no claude row under the shunt filter: {lines:?}"
    );
    for name in ["alpha", "beta", "cx-work", "spare", "k1"] {
        assert!(
            lines.iter().any(|line| line.contains(name)),
            "{name} drawn under the shunt filter: {lines:?}"
        );
    }
    assert!(
        accounts_title_row(&app, 60).ends_with(" 1 claude · 5 shunt ─╮"),
        "the meta counts every section: {:?}",
        accounts_title_row(&app, 60)
    );
}

/// The published cursor starts walk both bands: each pool row's rendered line
/// accounts for the codex spacer + heading AND the per-provider shunt headings,
/// so the page step (which reads these starts) can never land a selection off
/// its row, and the scrollbar's total counts every rendered line.
#[test]
fn the_page_step_and_scrollbar_walk_a_list_with_all_three_sections() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let (config, readings) = config_with(vec![profile("cl1", 80.0, 10.0, 3_600)], None, vec![]);
    let mut app = App::new(config);
    app.shunt_pool = sample_pool();

    // Full-height draw publishes the geometry the page step and scrollbar read.
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
        .expect("draw");
    // items: cl1, cx1, alpha, beta, cx-work, spare, k1.
    assert_eq!(
        *app.overview_selector.starts.borrow(),
        vec![0, 3, 6, 7, 9, 10, 12],
        "each pool row's rendered line crosses both bands"
    );
    assert_eq!(
        app.overview_selector.total.get(),
        13,
        "the scrollbar counts every rendered line, headings included"
    );

    // A short viewport with the cursor on the last pool row keeps the caret on
    // that row.
    app.overview_cursor = 6;
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).expect("terminal");
    term.draw(|f| draw_overview_accounts(f, f.area(), &app, &readings))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows[6].contains("❯") && rows[6].contains("k1"),
        "the last pool row follows: {rows:?}"
    );
    assert!(
        accounts_scrollbar_column(&app, 80, 8).contains('┃'),
        "the scrollbar thumb shows the overflow"
    );
}

// ── bounded Overview chain summary (tui-fix S3) ─────────────────────────────

/// A chain of live, unexhausted members — the only caption is the always-on
/// all-spent line, so the truncation arithmetic is pinned exactly.
fn chain_app(names: &[String]) -> App {
    let profiles: Vec<(Profile, ReadingsSnapshot)> =
        names.iter().map(|n| profile(n, 95.0, 20.0, 3600)).collect();
    let chain: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    App::new(config_with(profiles, None, chain).0)
}

#[test]
fn truncate_chain_keeps_captions_and_names_hidden_members() {
    let _home = crate::testutil::HomeSandbox::new();
    let names: Vec<String> = (0..28).map(|i| format!("a{i}")).collect();
    let app = chain_app(&names);
    let (full, member_count) = fallback_flow_lines(&app, 76, &app.readings_snapshot());
    assert_eq!(member_count, 28, "the builder returns its member count");
    assert_eq!(full.len(), 29, "28 members + the all-spent caption");
    // 11 inner rows: 9 members + the caption + the more-line.
    let cut = truncate_chain(full, member_count, 11);
    assert_eq!(cut.len(), 11);
    assert_eq!(
        line_text(cut.last().unwrap()),
        "  +19 more · → fallback tab",
        "the last row names the 19 hidden members"
    );
    assert!(
        line_text(&cut[8]).contains("a8"),
        "the ninth member still draws"
    );
    assert!(
        line_text(&cut[9]).contains("when all spent"),
        "the all-spent caption survives the cut"
    );
}

#[test]
fn truncate_chain_draws_every_member_that_fits_without_the_more_line() {
    let _home = crate::testutil::HomeSandbox::new();
    // 11 members = the panel's inner height at 80×24; the base drew all 11 and
    // the round-1 code hid two behind a `+2 more` line.
    let names: Vec<String> = (0..11).map(|i| format!("a{i}")).collect();
    let app = chain_app(&names);
    let (full, member_count) = fallback_flow_lines(&app, 76, &app.readings_snapshot());
    assert_eq!(full.len(), 12, "11 members + the all-spent caption");
    let cut = truncate_chain(full, member_count, 11);
    assert_eq!(cut.len(), 11, "every member fits, the caption drops");
    assert!(
        cut.iter().all(|l| !line_text(l).contains("more · ")),
        "no hidden-members line when every member fits"
    );
    assert!(
        line_text(&cut[10]).contains("a10"),
        "the eleventh member draws"
    );
}

#[test]
fn overview_chain_panel_names_hidden_members_on_a_short_terminal() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let _home = crate::testutil::HomeSandbox::new();
    let names: Vec<String> = (0..28).map(|i| format!("a{i}")).collect();
    let app = chain_app(&names);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let more = rows
        .iter()
        .find(|r| r.contains("more · "))
        .unwrap_or_else(|| panic!("the hidden-members line renders: {rows:#?}"));
    let content = more
        .strip_prefix("│ ")
        .unwrap_or(more)
        .trim_end_matches([' ', '│']);
    assert_eq!(
        content, "  +19 more · → fallback tab",
        "the whole last content row names the hidden members, got {content:?}"
    );
}

#[test]
fn overview_chain_panel_omits_the_more_line_when_everyone_fits() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let _home = crate::testutil::HomeSandbox::new();
    let names: Vec<String> = (0..3).map(|i| format!("a{i}")).collect();
    let app = chain_app(&names);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        !out.contains("more · "),
        "no hidden-members line when the chain fits"
    );
}

#[test]
fn overview_more_line_styles_the_arrow_as_a_key() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let names: Vec<String> = (0..28).map(|i| format!("a{i}")).collect();
    let app = chain_app(&names);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let buf = term.backend().buffer();
    crate::testutil::assert_prose_part(buf, "+19 more · → fallback tab", "→", true);
    // The words beside the arrow are the caption style, faint — pinned by
    // equality, not just "not accent".
    let faint = crate::tui::theme::text_faint_color();
    for word in ["+19 more", "fallback tab"] {
        let cells =
            crate::testutil::needle_cells(buf, word).unwrap_or_else(|| panic!("`{word}` missing"));
        for cell in cells {
            if cell.symbol() == " " {
                continue;
            }
            assert_eq!(cell.fg, faint, "`{word}` renders faint");
            assert!(
                !cell.modifier.contains(ratatui::style::Modifier::BOLD),
                "`{word}` is not bold"
            );
        }
    }
}

/// A hybrid's Overview bar text equals the
/// provider-derived percentage — `apply_usage` fills the snapshot from the
/// picker, so the Overview shows 10% (the provider bar), never the OAuth 100%.
#[test]
fn a_hybrids_overview_bar_shows_the_provider_percentage() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let name = ProfileName::from("h");
    let mut hybrid = crate::testutil::blank_profile(&name);
    hybrid.base_url = Some("https://example.com".to_string());
    hybrid.api_key = Some("key".to_string());
    hybrid.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    assert!(
        hybrid.usage_cache_is_third_party(),
        "fixture classifies third-party"
    );
    let (config, _readings) = config_with(
        vec![(hybrid, ReadingsSnapshot::default())],
        Some("h"),
        vec!["h"],
    );
    let mut app = App::new(config);
    app.usage_store
        .lock()
        .unwrap()
        .insert("h".into(), oauth_usage(100.0));
    app.third_party_usage_store
        .lock()
        .unwrap()
        .insert("h".into(), tp_bar(10.0));
    app.apply_usage();
    let readings = app.readings_snapshot();
    let widths = OverviewWidths::new(80, &app, &readings);
    let line = render_overview_row(&app, 0, &widths, false, true, &readings);
    let text = line_text(&line);
    assert_eq!(
        text, "  ● h             API                [█░░░░░░░░░] 10%       —              ",
        "the hybrid's Overview row renders the provider percentage"
    );
}

fn oauth_usage(pct: f64) -> crate::usage::UsageInfo {
    crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: pct,
            resets_at: Some(crate::usage::epoch_secs_to_iso(
                crate::usage::now_epoch_secs() + 3600,
            )),
        }),
        ..crate::usage::UsageInfo::default()
    }
}

fn tp_bar(pct: f64) -> crate::providers::ThirdPartyStats {
    crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![crate::providers::UsageBar {
            label: "5h".to_string(),
            pct,
            resets_at: Some(crate::usage::epoch_secs_to_iso(
                crate::usage::now_epoch_secs() + 3600,
            )),
            used: None,
            total: None,
            inference_quota: false,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

/// A hybrid active has no projected-switch ETA: its provider window has no OAuth
/// burn history to pair with, so `projected_switch` renders nothing even when a
/// burn series exists under the name (maintainer ruling 2026-09-10).
#[test]
fn a_hybrid_has_no_projected_switch_eta() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let name = ProfileName::from("h");
    let mut hybrid = crate::testutil::blank_profile(&name);
    hybrid.base_url = Some("https://example.com".to_string());
    hybrid.api_key = Some("key".to_string());
    hybrid.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    assert!(
        hybrid.usage_cache_is_third_party(),
        "fixture classifies third-party"
    );
    let (spare, spare_readings) = profile("spare", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(
        vec![
            (hybrid, ReadingsSnapshot::default()),
            (spare, spare_readings),
        ],
        Some("h"),
        vec!["h", "spare"],
    );
    let mut app = App::new(config);
    // The provider's live 5h bar at 92% — below the static threshold, but a
    // burn series would project it past.
    app.third_party_usage_store
        .lock()
        .unwrap()
        .insert("h".into(), tp_bar(92.0));
    app.apply_usage();
    let now = crate::usage::now_ms();
    app.history_cache.insert(
        "h".into(),
        vec![
            (now - 360_000, oauth_usage(32.0)),
            (now - 240_000, oauth_usage(52.0)),
            (now - 120_000, oauth_usage(72.0)),
        ],
    );
    assert_eq!(
        projected_switch(&app, &std::collections::HashSet::new()),
        None,
        "a hybrid active has no projected-switch ETA"
    );
}

/// Seed a live 5h window at `pct` into the OAuth usage store for `name`.
fn seed_5h(app: &mut App, name: &str, pct: f64) {
    #[allow(clippy::unwrap_used, reason = "test mutex poisoning is unrecoverable")]
    app.usage_store
        .lock()
        .unwrap()
        .insert(name.to_string(), oauth_usage(pct));
}

/// A switch-grade kick block lifted an hour out — the shape
/// `kick_rejected_names` counts as a rejected member.
fn kick_block(now: i64) -> crate::usage::KickBlock {
    crate::usage::KickBlock {
        streak: 2,
        rejected: true,
        until: Some(now + 3600),
        next_retry: now + 3600,
    }
}

/// Seed `name`'s in-memory burn history with a heavy climb so its rate projects
/// the active past the static threshold — the burn-aware projection the caption
/// reads its in-memory rate from.
fn seed_heavy_burn(app: &mut App, name: &str) {
    let now = crate::usage::now_ms();
    app.history_cache.insert(
        name.to_string(),
        vec![
            (now - 360_000, oauth_usage(36.0)),
            (now - 240_000, oauth_usage(56.0)),
            (now - 120_000, oauth_usage(76.0)),
        ],
    );
}

/// Seed a healthy two-member chain whose active `alpha` climbs 10 points every
/// 2 minutes (300 %/h) to 52.5% against a 95% threshold: 42.5 points at
/// 300 %/h is 510 s, so the hint reads `~8m`. The clock moving between the
/// fixture's stamps and the rate read only lowers the rate and raises the eta,
/// so the margin that matters is the 30 s up to 540 s (a fixture landing on
/// exactly 540 s rendered `8m` or `9m` by whether the clock moved a millisecond).
fn healthy_burning_alpha(bravo_pct: f64, switch_off_when_spent: bool) -> App {
    let (a, a_readings) = profile("alpha", 95.0, 52.5, 3600);
    let (b, b_readings) = profile("bravo", 95.0, bravo_pct, 3600);
    let (mut config, _readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("alpha"),
        vec!["alpha", "bravo"],
    );
    config.state.switch_off_when_spent = switch_off_when_spent;
    let mut app = App::new(config);
    #[allow(clippy::unwrap_used, reason = "test mutex poisoning is unrecoverable")]
    {
        let mut store = app.usage_store.lock().unwrap();
        store.insert("alpha".to_string(), oauth_usage(52.5));
        store.insert("bravo".to_string(), oauth_usage(bravo_pct));
        drop(store);
        let mut status = app.usage_status.lock().unwrap();
        status.insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
        status.insert("bravo".to_string(), crate::usage::FetchStatus::Fresh);
    }
    let now = crate::usage::now_ms();
    app.history_cache.insert(
        "alpha".to_string(),
        vec![
            (now - 360_000, oauth_usage(22.5)),
            (now - 240_000, oauth_usage(32.5)),
            (now - 120_000, oauth_usage(42.5)),
        ],
    );
    app
}

/// R1 (reviewer probe, ported): the DEFAULT config (burn-aware off) caption
/// names the member the walk lands on when a healthy active crosses its
/// threshold. The whole target row is pinned by equality.
#[test]
fn reviewer_r1_default_caption_names_the_next_member_for_a_healthy_burning_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = healthy_burning_alpha(10.0, false);
    let lines = fallback_flow_lines(&app, 120, &app.readings_snapshot()).0;
    assert_eq!(
        hinted_row_text(&lines),
        " ╰ 2 bravo   ▰▱▱▱▱▱▱▱▱▱▱▱    10% / 95%   ↲ ~8m",
        "the default-config caption names the member the walk lands on at the crossing"
    );
}

/// Wrap-off on and the only sibling spent: the walk signs everything out when
/// the healthy active crosses its threshold, so the DEFAULT config (burn-aware
/// off) warns with the `Off` caption ahead of it (cloudy, 2026-10-13: keep it).
#[test]
fn the_off_caption_warns_a_healthy_active_in_the_default_config() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = healthy_burning_alpha(100.0, true);
    let lines = fallback_flow_lines(&app, 120, &app.readings_snapshot()).0;
    let off: Vec<String> = lines
        .iter()
        .map(line_text)
        .filter(|t| t.contains("signs everything out"))
        .collect();
    assert_eq!(off, vec!["  signs everything out in ~8m".to_string()]);
}

/// K1: the projected-switch caption skips a switch-grade kick-rejected member
/// and names the clear one — the scheduler walk's `skip`, not the deleted UI
/// walk (which never skipped kick-rejected members). Default config; whole row
/// by equality.
#[test]
fn the_projected_switch_skips_a_kick_rejected_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("alpha", 95.0, 94.0, 3600);
    let (b, b_readings) = profile("bravo", 95.0, 10.0, 3600);
    let (c, c_readings) = profile("charlie", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(
        vec![(a, a_readings), (b, b_readings), (c, c_readings)],
        Some("alpha"),
        vec!["alpha", "bravo", "charlie"],
    );
    let mut app = App::new(config);
    seed_5h(&mut app, "alpha", 94.0);
    seed_5h(&mut app, "bravo", 10.0);
    seed_5h(&mut app, "charlie", 10.0);
    seed_heavy_burn(&mut app, "alpha");
    let now = crate::usage::now_epoch_secs();
    #[allow(clippy::unwrap_used, reason = "test mutex poisoning is unrecoverable")]
    {
        app.kick_blocks
            .lock()
            .unwrap()
            .insert("bravo".to_string(), kick_block(now));
        // Every member reads Fresh, so the fresh preference cannot discriminate:
        // only the walk's kick-rejected skip keeps bravo off the caption.
        app.usage_status
            .lock()
            .unwrap()
            .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
        app.usage_status
            .lock()
            .unwrap()
            .insert("bravo".to_string(), crate::usage::FetchStatus::Fresh);
        app.usage_status
            .lock()
            .unwrap()
            .insert("charlie".to_string(), crate::usage::FetchStatus::Fresh);
    }

    let lines = fallback_flow_lines(&app, 120, &app.readings_snapshot()).0;
    assert_eq!(
        hinted_row_text(&lines),
        " ╰ 3 charlie  ▰▱▱▱▱▱▱▱▱▱▱▱  10% / 95%   ↲ ~6s",
        "the caption must name the clear member, not the kick-rejected one"
    );
}

/// K2: the projected-switch caption prefers a FRESH reading over a dead-reading
/// member (deep `RateLimited` streak, no window). A dead-reading CANDIDATE is
/// never skipped — it only loses the freshness preference (`snapshot.fresh`) —
/// exactly as the deleted UI walk treated a non-Fresh member. Default config;
/// whole row by equality.
#[test]
fn the_projected_switch_prefers_a_fresh_reading_over_a_dead_reading_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("alpha", 95.0, 94.0, 3600);
    let (b, b_readings) = profile("bravo", 95.0, 10.0, 3600);
    let (c, c_readings) = profile("charlie", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(
        vec![(a, a_readings), (b, b_readings), (c, c_readings)],
        Some("alpha"),
        vec!["alpha", "bravo", "charlie"],
    );
    let mut app = App::new(config);
    seed_5h(&mut app, "alpha", 94.0);
    // bravo: windowless + deep RateLimited streak — its reading channel is dead.
    seed_5h(&mut app, "charlie", 10.0);
    seed_heavy_burn(&mut app, "alpha");
    #[allow(clippy::unwrap_used, reason = "test mutex poisoning is unrecoverable")]
    {
        app.usage_status
            .lock()
            .unwrap()
            .insert("bravo".to_string(), crate::usage::FetchStatus::RateLimited);
        app.poll_streaks.lock().unwrap().insert(
            "bravo".to_string(),
            crate::usage::StreakCounts {
                rate_limit: crate::usage::ACTIVE_CAP_MAX_STREAK + 1,
                refresh_fail: 0,
            },
        );
        app.usage_status
            .lock()
            .unwrap()
            .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
        app.usage_status
            .lock()
            .unwrap()
            .insert("charlie".to_string(), crate::usage::FetchStatus::Fresh);
    }

    let lines = fallback_flow_lines(&app, 120, &app.readings_snapshot()).0;
    assert_eq!(
        hinted_row_text(&lines),
        " ╰ 3 charlie  ▰▱▱▱▱▱▱▱▱▱▱▱  10% / 95%   ↲ ~6s",
        "the caption must prefer the fresh member over the dead-reading one"
    );
}

/// The simulated crossing reading (the active's utilization set to its
/// threshold) reads exhausted under burn-aware ON with the DEFAULT floor (98,
/// above the threshold 95): `util >= threshold` is the static disjunct of
/// `is_exhausted_projected`, so the floor never blocks the crossing projection.
#[test]
fn the_projected_switch_names_the_next_member_under_burn_aware_default_floor() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("alpha", 95.0, 94.0, 3600);
    let (b, b_readings) = profile("bravo", 95.0, 10.0, 3600);
    let (mut config, _readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("alpha"),
        vec!["alpha", "bravo"],
    );
    config.state.burn_aware_switching = true;
    let mut app = App::new(config);
    seed_5h(&mut app, "alpha", 94.0);
    seed_5h(&mut app, "bravo", 10.0);
    seed_heavy_burn(&mut app, "alpha");
    #[allow(clippy::unwrap_used, reason = "test mutex poisoning is unrecoverable")]
    {
        app.usage_status
            .lock()
            .unwrap()
            .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
        app.usage_status
            .lock()
            .unwrap()
            .insert("bravo".to_string(), crate::usage::FetchStatus::Fresh);
    }
    let lines = fallback_flow_lines(&app, 120, &app.readings_snapshot()).0;
    assert_eq!(
        hinted_row_text(&lines),
        " ╰ 2 bravo   ▰▱▱▱▱▱▱▱▱▱▱▱  10% / 95%   ↲ ~6s",
        "burn-aware with the default floor still projects the crossing"
    );
}

/// m1: the caption's live key-rejected fold — a live `third_party_broken`
/// (key-rejected) sibling is broken, so the walk skips it and the hint rides
/// the next clear member. Removing the fold lets the walk name the rejected
/// member. Whole row by equality.
#[test]
fn the_projected_switch_skips_a_live_key_rejected_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("alpha", 95.0, 94.0, 3600);
    let (mut bravo, bravo_readings) = third_party_profile(10.0, 10.0);
    bravo.name = "bravo".into();
    let fp = crate::usage::profile_credential_fingerprint(&bravo).expect("fingerprint");
    let (c, c_readings) = profile("charlie", 95.0, 10.0, 3600);
    let (config, _readings) = config_with(
        vec![(a, a_readings), (bravo, bravo_readings), (c, c_readings)],
        Some("alpha"),
        vec!["alpha", "bravo", "charlie"],
    );
    let mut app = App::new(config);
    seed_5h(&mut app, "alpha", 94.0);
    seed_5h(&mut app, "charlie", 10.0);
    seed_heavy_burn(&mut app, "alpha");
    #[allow(clippy::unwrap_used, reason = "test mutex poisoning is unrecoverable")]
    {
        // bravo's provider leg: a clear 5h bar + Fresh status, so removing the
        // key-rejected fold would make the walk pick it.
        app.third_party_usage_store
            .lock()
            .unwrap()
            .insert("bravo".to_string(), tp_bar(10.0));
        app.third_party_status
            .lock()
            .unwrap()
            .insert("bravo".to_string(), crate::usage::FetchStatus::Fresh);
        app.third_party_broken
            .lock()
            .unwrap()
            .insert("bravo".to_string(), fp);
        app.usage_status
            .lock()
            .unwrap()
            .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
        app.usage_status
            .lock()
            .unwrap()
            .insert("charlie".to_string(), crate::usage::FetchStatus::Fresh);
    }
    let lines = fallback_flow_lines(&app, 120, &app.readings_snapshot()).0;
    assert_eq!(
        hinted_row_text(&lines),
        " ╰ 3 charlie  ▰▱▱▱▱▱▱▱▱▱▱▱  10% / 95%   ↲ ~6s",
        "the caption skips the live key-rejected sibling and names the clear one"
    );
}

/// Desktop pin: the DeepSeek balance cell renders byte-identical to the pre-S9
/// render at 80/160 (the balance fits whole with brackets, padded to the 5h
/// column).
#[test]
fn the_deepseek_balance_cell_desktop_matches_the_pre_s9_render() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, readings) =
        config_with(vec![deepseek_profile("ds", &["1132.60 CNY"])], None, vec![]);
    let app = App::new(config);
    for (terminal, expected) in [
        (80u16, "[1132.60 CNY]    "),
        (160u16, "[1132.60 CNY]             "),
    ] {
        let widths = OverviewWidths::new(terminal.saturating_sub(4), &app, &readings);
        let row = render_overview_row(&app, 0, &widths, false, false, &readings);
        let cell = five_hour_cell_text(&widths, true, &row);
        assert_eq!(cell, expected, "balance cell at {terminal} cols");
    }
}

/// Desktop pin: the all-spent caption renders byte-identical to the pre-S9
/// render at the desktop inner widths (76/156).
#[test]
fn the_stay_caption_desktop_rows_match_the_pre_s9_render() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let (config, readings) = config_with(vec![a], Some("a"), vec!["a"]);
    let app = App::new(config);
    for w in [76usize, 156] {
        let lines = fallback_flow_lines(&app, w, &readings).0;
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let start = texts
            .iter()
            .position(|t| t.contains("[ stay ]"))
            .unwrap_or_else(|| panic!("stay caption missing at inner {w}: {texts:?}"));
        let got: Vec<String> = texts[start..].to_vec();
        assert_eq!(
            got,
            vec!["  [ stay ] on the active account when all spent"],
            "stay caption at inner {w} cells"
        );
    }
}

// ── tps: the overview `t/s` column ───────────────────────────────────────

/// A fresh snapshot giving `a` a live speed of 92.4 and `b` none.
fn tps_overview_snapshot(age_ms: u64) -> crate::tps::Snapshot {
    let stamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(crate::usage::now_ms() - age_ms).unwrap(),
    )
    .unwrap()
    .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
    .to_string();
    let mut snap = crate::tps::Snapshot {
        version: crate::tps::CACHE_VERSION,
        generated_at: stamp,
        ..crate::tps::Snapshot::default()
    };
    snap.accounts.insert(
        "a".into(),
        crate::tps::Figures {
            live: crate::tps::Figure {
                rate: 67.2,
                speed: Some(92.4),
            },
            ..Default::default()
        },
    );
    snap
}

/// The `t/s` column sits after `live`: a cell carries the account's live speed,
/// right-aligned, blank where absent, and the whole column drops before the
/// 7d reset stamp (and before the live column).
#[test]
fn the_overview_tps_column_renders_blank_when_absent_and_drops_first() {
    let _home = crate::testutil::HomeSandbox::new();
    let (a, a_readings) = profile("a", 80.0, 42.0, 3600);
    let (b, b_readings) = profile("b", 80.0, 10.0, 3600);
    let (config, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    let mut app = App::new(config);
    app.set_tps(Some(tps_overview_snapshot(1_000)));

    // Both columns fit at a wide width, with the full 7d stamp present; the
    // header closes with the speed column.
    let wide = OverviewWidths::new(200, &app, &readings);
    assert_eq!((wide.live, wide.tps), (LIVE_W, TPS_W));
    assert!(
        wide.seven_day >= 26,
        "the full 7d stamp holds: {}",
        wide.seven_day
    );
    assert!(
        line_text(&overview_header(&wide, false)).ends_with("t/s "),
        "the header closes with the speed column"
    );

    // `a` carries its speed right-aligned, `b` a blank cell — never a dash for
    // a figure the producer did not read.
    assert_eq!(
        line_text(&Line::from(super::tps_cell(&app, "a", TPS_W))),
        "  92"
    );
    assert_eq!(
        line_text(&Line::from(super::tps_cell(&app, "b", TPS_W))),
        "    "
    );

    // The column drops whole with the 7d stamp: at 103 the stamp and the
    // column hold; two columns narrower the stamp has shed and so has the t/s
    // column (the live column still fits).
    let stamped = OverviewWidths::new(103, &app, &readings);
    assert!(stamped.seven_day >= 26, "{}", stamped.seven_day);
    assert_eq!(stamped.tps, TPS_W);
    let shed = OverviewWidths::new(101, &app, &readings);
    assert!(shed.seven_day < 26, "{}", shed.seven_day);
    assert_eq!(shed.tps, 0, "the t/s column drops with the 7d stamp");
    assert_eq!(shed.live, LIVE_W, "the live column still holds");

    // The row renders the right-aligned speed under the t/s header, and a
    // blank cell for the account without one.
    let row_a = line_text(&render_overview_row(
        &app, 0, &wide, false, false, &readings,
    ));
    assert!(row_a.contains("  92"), "{row_a:?}");
}

/// The cell's figure abbreviates to a whole `k` once the formatted text would
/// exceed the 4-cell column, capped at `999k`, never truncated into `…`.
#[test]
fn the_tps_cell_abbreviates_with_k_and_never_truncates() {
    assert_eq!(crate::tps::format_figure_capped(9_999.4, TPS_W), "9999");
    assert_eq!(crate::tps::format_figure_capped(9_999.6, TPS_W), "10k");
    assert_eq!(crate::tps::format_figure_capped(12_800.0, TPS_W), "13k");
    assert_eq!(crate::tps::format_figure_capped(1_200_000.0, TPS_W), "999k");
}

/// The row never exceeds the pane in any reset mode: the wall-clock bonus
/// yields to the t/s column only when the post-bonus row still fits. Swept at
/// a whole reading and at a two-decimal one (which widens the 5h tier's
/// excess), so the saturation path above excess 0 is charged too.
#[test]
fn the_overview_row_never_exceeds_the_pane_in_any_reset_mode() {
    use crate::profile::ResetDisplay;
    let _home = crate::testutil::HomeSandbox::new();
    for mode in [
        ResetDisplay::Relative,
        ResetDisplay::Clock,
        ResetDisplay::Both,
    ] {
        for util in [42.0, 73.42] {
            let (a, a_readings) = profile("a", 80.0, util, 3600);
            let (mut config, readings) = config_with(vec![(a, a_readings)], Some("a"), vec![]);
            config.state.reset_display = Some(mode);
            let mut app = App::new(config);
            app.set_tps(Some(tps_overview_snapshot(1_000)));
            for w in 95..=140u16 {
                let widths = OverviewWidths::new(w, &app, &readings);
                let row_w = super::fixed_overview_width_cols(
                    widths.name,
                    widths.kind,
                    widths.five_hour,
                    widths.seven_day,
                    widths.live,
                    widths.tps,
                    widths.gap,
                ) + super::TIMER_SLOT;
                assert!(
                    row_w <= w as usize,
                    "{mode:?} at {w} (util {util}): {row_w} > {w}"
                );
            }
        }
    }
}
