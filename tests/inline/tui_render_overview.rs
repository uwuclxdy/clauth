//! `fallback_flow_lines`'s all-exhausted "resumes: <name> in ~<eta>" caption
//! (issue #10 follow-up) — the sibling of the "switching to <name> in ~<eta>"
//! projection line, driven by `crate::fallback::soonest_resume`.
//! Plus the overview-row state cues: marker precedence + countdown fetch cue.

use super::*;
use ratatui::style::Modifier;

use crate::fallback::BlockedReason;
use crate::profile::{AppState, ClaudeCredentials, OAuthToken, ProfileName};
use crate::usage::{FetchStatus, UsageInfo, epoch_secs_to_iso, now_epoch_secs};
use std::collections::BTreeMap;

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
    let p = third_party_profile(60.0, 30.0);
    let config = config_with(vec![p], None, vec![]);
    let app = App::new(config);
    let profile = &app.config().profiles[0];
    let (five, seven) = overview_windows(profile);
    let five = five.expect("5h bar synthesizes a window");
    let seven = seven.expect("7d bar synthesizes a window");

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
    )
    .expect("a half-elapsed 5h window yields an avg pace");
    let seven_rate = drain_rate(
        &app,
        &crate::profile::ProfileName::from("tp"),
        profile,
        LABEL_7D,
        &seven,
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

/// An OAuth 5h window keeps the recency-weighted recent burn, not the avg pace:
/// with no history recorded, it stays uncolored rather than falling back.
#[test]
fn drain_rate_oauth_five_hour_uses_recent_burn() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 60.0, 9_000);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let p = &app.config().profiles[0];
    let w = p.usage.as_ref().unwrap().five_hour.clone().unwrap();
    assert!(
        drain_rate(
            &app,
            &crate::profile::ProfileName::from("a"),
            p,
            LABEL_5H,
            &w
        )
        .is_none(),
        "no recorded history → no rate, rather than an avg-pace fallback",
    );
}

/// An api-key/provider profile: no `UsageInfo`, so its overview 5h/7d windows
/// are synthesized from the provider bars. Both bars sit exactly half-elapsed,
/// which is enough for `window_avg_pace_per_day` to have a pace to report.
fn third_party_profile(five_pct: f64, seven_pct: f64) -> Profile {
    let bar = |label: &str, pct: f64, reset_secs: i64| crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some(reset_in(reset_secs)),
        used: None,
        total: None,
    };
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
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: Some(crate::providers::ThirdPartyStats {
            is_available: true,
            rows: Vec::new(),
            bars: vec![
                bar(LABEL_5H, five_pct, 5 * 3600 / 2),
                bar(LABEL_7D, seven_pct, 7 * 86_400 / 2),
            ],
            plan: None,
            endpoint: None,
            best_effort: false,
        }),
    }
}

/// A DeepSeek api-key profile with cached balance rows built from `totals`
/// (e.g. `["1.71 USD", "100.00 CNY"]`). Each total is preceded by a heading row
/// whose currency is extracted from the total value. An empty slice produces an
/// empty-rows snapshot, matching an account whose balance fetch has not landed.
fn deepseek_profile(name: &str, totals: &[&str]) -> Profile {
    let mut rows = Vec::new();
    for t in totals {
        if let Some((_, currency)) = t.rsplit_once(' ') {
            rows.push(crate::providers::StatRow {
                label: format!("{currency} balance"),
                value: String::new(),
                kind: crate::providers::StatRowKind::Heading,
            });
        }
        rows.push(crate::providers::StatRow {
            label: crate::providers::DEEPSEEK_BALANCE_ROW_LABEL.into(),
            value: (*t).to_string(),
            kind: crate::providers::StatRowKind::Body,
        });
    }
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
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: Some(crate::providers::Provider::DeepSeek),
        third_party_usage: Some(crate::providers::ThirdPartyStats {
            is_available: true,
            rows,
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
        }),
    }
}

/// A DeepSeek profile whose cached rows come from a captured
/// `third_party_cache.json` (see the `CAPTURED_*_DS_CACHE` constants in
/// [`crate::testutil`]): the bytes go through the production cache writer and
/// reader into the field a live app's `apply_usage` fills, so the render path
/// is driven by captured bytes rather than a hand-built `ThirdPartyStats`.
fn deepseek_profile_from_cache(name: &str, captured: &str) -> Profile {
    let base = deepseek_profile(name, &[]);
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
    Profile {
        name: name.into(),
        base_url: Some("https://api.deepseek.com/anthropic".into()),
        api_key: Some("k".into()),
        provider: Some(crate::providers::Provider::DeepSeek),
        third_party_usage: Some(stats),
        ..deepseek_profile(name, &[])
    }
}

/// A chain-eligible OAuth profile with a live 5h window at `util`%, resetting
/// in `reset_secs`.
fn profile(name: &str, threshold: f64, util: f64, reset_secs: i64) -> Profile {
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
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: Some(reset_in(reset_secs)),
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>, chain: Vec<&str>) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

/// Flattens a line's spans to plain text for substring assertions.
fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn resumes_line(lines: &[Line<'static>]) -> Option<String> {
    lines.iter().map(line_text).find(|t| t.contains("resumes:"))
}

// Wrap mode: the active profile itself is exhausted and stays put (no sink,
// `next_target` returns `None`) — previously silent. b resets sooner than a.
#[test]
fn all_exhausted_wrap_mode_shows_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 100.0, 1800);
    let config = config_with(vec![a, b], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60);
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
    let a = profile("a", 95.0, 100.0, 900);
    let b = profile("b", 95.0, 100.0, 3600);
    let mut config = config_with(vec![a, b], None, vec!["a", "b"]);
    config.state.switch_off_when_spent = true;
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60);
    let hint = resumes_line(&lines)
        .expect("resumes hint must render even with no active profile (wrap-off cleared it)");
    assert!(hint.contains("resumes: a in ~"), "{hint}");
}

// b still has headroom — the chain is not all-exhausted, so the caption must
// stay hidden (recovery would relink b on the next tick regardless).
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let config = config_with(vec![a, b], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60);
    assert!(
        resumes_line(&lines).is_none(),
        "must not show when the chain isn't fully exhausted"
    );
}

// Nobody near their threshold at all — the ordinary healthy-chain case.
#[test]
fn healthy_chain_hides_resumes_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let b = profile("b", 95.0, 5.0, 3600);
    let config = config_with(vec![a, b], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60);
    assert!(resumes_line(&lines).is_none());
}

// ── overview row state cues ──────────────────────────────────────────────

/// Marker column: a broken login (×) outranks both the bell (!) and the
/// active dot (●) — usage alerts are moot until re-login.
#[test]
fn broken_login_marker_outranks_bell_and_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let a = profile("a", 95.0, 10.0, 3600);
    let mut config = config_with(vec![a], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
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
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], None, vec![]);
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true);
    let widths = OverviewWidths::new(80, &app);
    let text = line_text(&render_overview_row(&app, 0, &widths, false, true));
    assert!(text.contains('!'), "{text}");
    assert!(!text.contains('×'), "{text}");
}

/// A dead / mis-filled long-lived token (⊘) outranks bell (!) and active (●):
/// the next switch would sign sessions out, so it beats a usage alert.
#[test]
fn token_danger_marker_outranks_bell_and_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec![]); // active
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true); // bell also fired
    app.session_tokens
        .insert("a".into(), crate::claude::SessionTokenStatus::NotLongLived);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
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
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.usage.as_mut().unwrap().plan = Some(PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    });
    let mut config = config_with(vec![a], Some("a"), vec![]); // also active
    config.state.auth_broken.push("a".into()); // also auth-broken
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true); // bell also fired
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
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
    let a = profile("a", 95.0, 10.0, 3600);
    let mut config = config_with(vec![a], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.session_tokens
        .insert("a".into(), crate::claude::SessionTokenStatus::NotLongLived);
    let widths = OverviewWidths::new(80, &app);
    let text = line_text(&render_overview_row(&app, 0, &widths, false, true));
    assert!(text.contains('×'), "broken login wins: {text}");
    assert!(!text.contains('⊘'), "token marker yields to ×: {text}");
}

/// A live long-lived token raises no marker; an expired one raises the ⊘ danger marker.
#[test]
fn long_lived_token_expired_marks() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::claude::SessionTokenStatus as S;
    let day = 86_400_000_i64;
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], None, vec![]);
    let mut app = App::new(config);
    let widths = OverviewWidths::new(120, &app);

    app.session_tokens
        .insert("a".into(), S::LongLived(Some(now_ms() as i64 + 340 * day)));
    let live = line_text(&render_overview_row(&app, 0, &widths, false, true));
    assert!(
        !live.contains('⊘'),
        "a live token raises no danger marker: {live}"
    );

    app.session_tokens
        .insert("a".into(), S::LongLived(Some(now_ms() as i64 - day)));
    let dead = line_text(&render_overview_row(&app, 0, &widths, false, true));
    assert!(dead.contains('⊘'), "expired token raises ⊘: {dead}");
}

/// The stale-data cue lives on the refresh countdown now — an underlined name
/// would double-signal, and the bar brackets stay plain dim.
#[test]
fn cached_row_colors_countdown_amber_and_underlines_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.fetch_status = Some(FetchStatus::Cached);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert("a".to_string(), now_ms() + 30_000);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
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
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.fetch_status = Some(FetchStatus::Failed);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert("a".to_string(), now_ms() + 30_000);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
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
    let config = config_with(vec![third_party_profile(60.0, 30.0)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app);
    assert!(
        widths.five_hour >= 26 && widths.seven_day >= 26,
        "test needs both columns wide enough to render a (reset) suffix",
    );
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true));
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
    let mut a = profile("a", 95.0, 60.0, 9_000);
    a.usage.as_mut().unwrap().seven_day = Some(UsageWindow {
        utilization: 30.0,
        resets_at: Some(reset_in(7 * 86_400 / 2)),
    });
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app);
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true));
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
    let a = profile("ax-main", 95.0, 10.0, 3600);
    let b = profile("ax-backup", 95.0, 20.0, 3600);
    let config = config_with(vec![a, b], Some("ax-main"), vec![]);
    let app = App::new(config);
    for width in 34u16..=200 {
        let w = OverviewWidths::new(width, &app);
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
fn credentialed_profile(name: &str, subscription_type: &str) -> Profile {
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
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
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
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// The credentialed pulse arm must clamp the type-column label to
/// `widths.kind` exactly like the non-credentialed `fixed` arm does. A
/// long label ("Enterprise", 10 chars) must not overflow a narrow `kind`
/// column (6 chars) and bleed into the following gap/timer columns.
#[test]
fn credentialed_long_label_clamps_to_kind_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = credentialed_profile("acct", "enterprise");
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(60, &app);
    assert_eq!(
        widths.kind, 6,
        "test assumes a 6-wide kind column at this pane width"
    );

    let line = render_overview_row(&app, 0, &widths, false, true);
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
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.disabled = true;
    let mut b = profile("b", 95.0, 10.0, 3600);
    // Give both a real FETCHED Pro tier so the TYPE column reads a genuine tier —
    // this test is about dimming keeping whatever tier is real, not about the
    // fallback tier a credential-less profile happens to default to.
    for p in [&mut a, &mut b] {
        p.usage.as_mut().unwrap().plan = Some(crate::usage::PlanInfo {
            tier: crate::usage::PlanTier::Pro,
            subscription_status: None,
        });
    }
    let config = config_with(vec![a, b], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(80, &app);

    let disabled_line = render_overview_row(&app, 0, &widths, false, true);
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

    let enabled_line = render_overview_row(&app, 1, &widths, false, true);
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
        }),
    }
}

/// A disabled row goes inert END TO END, not just its name: the marker glyph,
/// the type cell, and both window bars all flatten to `theme::dim()`. The
/// glyphs and numbers stay — cloudy-tui never lets state ride on hue alone, and
/// the figures are the last real reading — it is only the semantic color that
/// lies once the data is frozen. An enabled sibling in the same config keeps
/// every hue, which is what proves the flattening is per-row.
#[test]
fn disabled_row_flattens_every_semantic_hue_to_dim() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("a", 95.0, 90.0, 3600);
    a.disabled = true;
    a.credentials = Some(oauth_creds());
    let mut b = profile("b", 95.0, 90.0, 3600);
    b.credentials = Some(oauth_creds());
    // Both rows must actually REACH the marker branch, or the flattening
    // assertion silently skips it: a disabled, non-active, unbroken profile
    // falls through to a blank marker with no fg to flatten. Marking both
    // auth-broken puts the `×` (DANGER) on each.
    let mut config = config_with(vec![a, b], None, vec![]);
    config.state.auth_broken.push("a".into());
    config.state.auth_broken.push("b".into());
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app);

    let non_dim = |line: &Line<'static>| -> Vec<String> {
        line.spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .filter(|s| s.style.fg.is_some() && s.style.fg != theme::dim().fg)
            .map(|s| s.content.to_string())
            .collect()
    };

    let disabled_line = render_overview_row(&app, 0, &widths, false, true);
    assert_eq!(
        non_dim(&disabled_line),
        Vec::<String>::new(),
        "every colored span on a disabled row must flatten to dim"
    );

    // The control row must still carry hue, or the assertion above is vacuous.
    let enabled_line = render_overview_row(&app, 1, &widths, false, true);
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
    // below it. The tier auto-detects from `$COLORTERM`, which CI leaves unset,
    // so an unpinned tier renders every row flat. That makes the assertion
    // below vacuous and fails its control.
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.disabled = true;
    a.credentials = Some(oauth_creds());
    let mut b = profile("b", 95.0, 10.0, 3600);
    b.credentials = Some(oauth_creds());
    let config = config_with(vec![a, b], None, vec![]);

    // `pulse_name_spans` keys off `App::anim_ms`, so two Apps with different
    // pinned phases sample different points of the wave. 450ms is the crest of
    // the 900ms sweep, where the envelope peaks: the phase furthest from the
    // flat 0ms frame, so a surviving pulse shows up at its widest rather than at
    // some near-zero lean that rounds back to base.
    let snapshot = |idx: usize| -> Vec<(String, Option<ratatui::style::Color>)> {
        let mut app = App::new(config.clone());
        app.anim_phase_ms = Some(450 * idx as u64);
        let widths = OverviewWidths::new(110, &app);
        render_overview_row(&app, 0, &widths, false, true)
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
        let widths = OverviewWidths::new(110, &app);
        render_overview_row(&app, 1, &widths, false, true)
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
    // The wave is Full-tier only; an unpinned tier renders every row flat and
    // makes the equality below vacuous. Same guard the disabled-row pin carries.
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);
    // `something_new` is a claim clauth cannot classify, so the row has no tier
    // at all; `max` is the same row WITH one.
    let config = config_with(
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
        let widths = OverviewWidths::new(110, &app);
        render_overview_row(&app, idx, &widths, false, true)
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
    let mut disabled = credentialed_profile("c", "something_new");
    disabled.disabled = true;
    let config = config_with(
        vec![
            credentialed_profile("a", "something_new"),
            disabled,
            Profile::new(
                "d".to_string(),
                Some("https://api.deepseek.com/anthropic".to_string()),
                Some("sk-fixture".to_string()),
            ),
            Profile::new("e".to_string(), None, None),
        ],
        None,
        vec![],
    );

    let cell = |idx: usize| -> (String, Option<ratatui::style::Color>) {
        let mut app = App::new(config.clone());
        app.anim_phase_ms = Some(0);
        let widths = OverviewWidths::new(110, &app);
        let span = render_overview_row(&app, idx, &widths, false, true).spans[KIND_SPAN].clone();
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
/// zero and then claim a refresh forever. The slot renders blank — at full
/// width, so no column downstream shifts.
#[test]
fn disabled_row_blanks_the_refresh_countdown_at_full_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.disabled = true;
    let b = profile("b", 95.0, 10.0, 3600);
    let config = config_with(vec![a, b], None, vec![]);
    let app = App::new(config);
    // Both profiles carry a live countdown in the shared map.
    if let Ok(mut m) = app.next_refresh_per_profile.lock() {
        m.insert("a".to_string(), now_ms() + 42_000);
        m.insert("b".to_string(), now_ms() + 42_000);
    }
    let widths = OverviewWidths::new(110, &app);

    let disabled_line = render_overview_row(&app, 0, &widths, false, true);
    let enabled_line = render_overview_row(&app, 1, &widths, false, true);
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
    let stale = profile("a", 95.0, 73.0, -60); // reset 60s in the past
    let live = profile("b", 95.0, 73.0, 3600); // reset in 1h
    let config = config_with(vec![stale, live], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app);

    let stale_line = render_overview_row(&app, 0, &widths, false, true);
    let live_line = render_overview_row(&app, 1, &widths, false, true);
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
    let mut a = profile("a", 95.0, 73.0, -60); // disabled + past-reset
    a.disabled = true;
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app);

    let line = render_overview_row(&app, 0, &widths, false, true);
    let non_dim: Vec<String> = line
        .spans
        .iter()
        .filter(|s| !s.content.trim().is_empty())
        .filter(|s| s.style.fg.is_some() && s.style.fg != theme::dim().fg)
        .map(|s| s.content.to_string())
        .collect();
    assert_eq!(
        non_dim,
        Vec::<String>::new(),
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

/// The projected switch target carries the compact `↩ ~eta` hint on its OWN row
/// (not a trailing caption), parked at the shared trailer column just past the
/// content — NOT flung out to the panel's right edge.
#[test]
fn chain_row_switch_hint_rides_the_target_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec!["a"]);
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
            thr_w: 3,
            reason: None,
            switch_eta: Some(7200),
        },
    );
    let base = row.base_width();
    let line = row.into_line(base + TRAILER_GAP, 60);
    let text = line_text(&line);
    assert!(text.contains("↩ ~"), "target row carries the hint: {text}");
    let hint_w = Span::raw(format!("↩ ~{}", humanize_duration(7200))).width();
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

/// A projected switch LANDING on the preferred (home) member carries the `⌂`
/// homecoming glyph, while a switch onto any other member carries the plain `↩`.
/// Pins the wording that distinguishes a return from an exhaustion hop (spec
/// item 6) — an inverted glyph (⌂/↩ swapped) would otherwise ship green.
#[test]
fn chain_row_marks_a_homecoming_onto_preferred_with_the_house_glyph() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut home = profile("home", 95.0, 10.0, 3600);
    home.preferred = true;
    let plain = profile("plain", 95.0, 10.0, 3600);
    let config = config_with(vec![home, plain], Some("plain"), vec!["home", "plain"]);
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
                thr_w: 3,
                reason: None,
                switch_eta: Some(7200),
            },
        );
        let base = row.base_width();
        line_text(&row.into_line(base + TRAILER_GAP, 60))
    };

    let home_hint = hint("home");
    assert!(
        home_hint.contains('⌂') && !home_hint.contains('↩'),
        "a switch onto the preferred member reads as a homecoming: {home_hint}",
    );
    let plain_hint = hint("plain");
    assert!(
        plain_hint.contains('↩') && !plain_hint.contains('⌂'),
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
    let short = profile("ab", 100.0, 10.0, 3600);
    let long = profile("a-much-longer-name", 100.0, 10.0, 3600);
    let mut config = config_with(
        vec![short, long],
        Some("ab"),
        vec!["ghost", "ab", "a-much-longer-name"],
    );
    config.state.auth_broken.push("ab".into());
    let app = App::new(config);

    let wide = 120;
    let lines = fallback_flow_lines(&app, wide);
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

/// Thresholds of differing digit counts left-pad so the `%` signs stack
/// (cloudy-tui numeric-column alignment), instead of leaving a ragged edge
/// between a `95%` row and a `100%` row.
#[test]
fn chain_rows_align_the_threshold_percent_column() {
    let _home = crate::testutil::HomeSandbox::new();
    let ninety_five = profile("a", 95.0, 10.0, 3600);
    let hundred = profile("b", 100.0, 10.0, 3600);
    let config = config_with(vec![ninety_five, hundred], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let texts: Vec<String> = fallback_flow_lines(&app, 60)
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

/// A row can be BOTH the projected switch target and blocked: `next_target`'s
/// headroom walk only prefers a fresh candidate and falls through to a
/// stale-but-unexhausted one (`is_exhausted` ignores `fetch_status`), so a
/// stale/soft-blocked member can still be `To`'s pick. With room for both, the
/// row shows the hint AND the marker rather than silently dropping the
/// imminent-switch projection.
#[test]
fn chain_row_shows_both_switch_hint_and_reason_marker_when_they_fit() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec!["a"]);
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
            thr_w: 3,
            reason: Some(BlockedReason::AuthBroken),
            switch_eta: Some(7200),
        },
    );
    let col = row.base_width() + TRAILER_GAP;
    let text = line_text(&row.into_line(col, 60));
    assert!(text.contains('×'), "auth-broken shows the × marker: {text}");
    assert!(text.contains("↩ ~"), "and the switch hint: {text}");
}

/// Too narrow for the pair: the marker (the persistent block signal) survives
/// and the hint drops rather than the row overflowing or the marker vanishing.
/// Derives the width thresholds from the row's own natural content width
/// (`base_width`) instead of hand-counting cells, which is brittle against
/// gauge/figure formatting changes.
#[test]
fn chain_row_drops_switch_hint_before_reason_marker_when_narrow() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec!["a"]);
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
                thr_w: 3,
                reason: Some(BlockedReason::AuthBroken),
                switch_eta: Some(7200),
            },
        )
    };
    let col = build().base_width() + TRAILER_GAP;
    let marker_w = reason_marker(&BlockedReason::AuthBroken).width();
    let hint_w = Span::raw(format!("↩ ~{}", humanize_duration(7200))).width();

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
    assert!(!text.contains('↩'), "hint drops first: {text}");
}

/// End to end: an auth-broken chain member surfaces its × marker in the overview
/// fallback panel — exercises the kick-lift read + `blocked_reason` wiring.
#[test]
fn fallback_panel_marks_a_blocked_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 10.0, 3600);
    let mut config = config_with(vec![a], Some("a"), vec!["a"]);
    config.state.auth_broken.push("a".into());
    let app = App::new(config);
    let joined = fallback_flow_lines(&app, 60)
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
    line_text(row).chars().skip(col).collect()
}

/// The column answers "how many `clauth start` sessions are on this account",
/// with `⇄` marking that at least one of them can be moved by the chain. It is
/// DISTINCT from the leading `●`, which marks the one profile a bare `claude`
/// authenticates as — an account can carry either, both, or neither. That split
/// is why the header reads `live`: `active` is the `●` sense app-wide.
#[test]
fn the_live_column_counts_live_sessions_and_marks_chain_followers() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(config_with(
        vec![
            profile("main", 95.0, 10.0, 3600),
            profile("spare", 95.0, 20.0, 3600),
        ],
        Some("main"),
        vec![],
    ));
    app.live_sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "main", true),
        live_row("4242-1", "main", false),
        live_row("4242-2", "spare", false),
    ]);

    let widths = OverviewWidths::new(160, &app);
    let main = render_overview_row(&app, 0, &widths, false, false);
    let spare = render_overview_row(&app, 1, &widths, false, false);

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
    let mut p = profile("main", 95.0, 10.0, 3600);
    if let Some(ref mut usage) = p.usage {
        // 37200s = 10h 20m → `humanize_duration` emits the 7-char form.
        usage.seven_day = Some(UsageWindow {
            utilization: 95.0,
            resets_at: Some(reset_in(37_200)),
        });
    }
    let mut app = App::new(config_with(vec![p], Some("main"), vec![]));
    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "main", true)]);

    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);

    assert_eq!(
        live_cell_text(&widths, &row),
        "1  ⇄",
        "the 7d reset suffix must not push the live cell past its header"
    );
}

/// Zero renders as nothing — cloudy-tui hides a zero count rather than printing
/// it, and a table full of `0`s would drown the accounts that do host something.
#[test]
fn an_account_with_no_live_sessions_renders_a_blank_live_cell() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    ));
    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "other", true)]);

    let widths = OverviewWidths::new(160, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);

    assert_eq!(live_cell_text(&widths, &row), "    ");
}

/// The column is budgeted on WIDTH alone. Were it budgeted on whether anything
/// is live, the whole table would reflow the moment someone ran `clauth start`
/// and reflow back when that session exited — so an empty fleet must lay the
/// table out exactly as a busy one does.
#[test]
fn the_live_column_holds_its_place_while_nothing_is_live() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    ));

    let idle_widths = OverviewWidths::new(160, &app);
    let idle_header = line_text(&overview_header(&idle_widths, false));
    let idle_row = line_text(&render_overview_row(&app, 0, &idle_widths, false, false));

    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "main", true)]);
    let busy_widths = OverviewWidths::new(160, &app);
    let busy_header = line_text(&overview_header(&busy_widths, false));
    let busy_row = line_text(&render_overview_row(&app, 0, &busy_widths, false, false));

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
    let mut app = App::new(config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    ));
    app.live_sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "main", true)]);

    for width in 34u16..=200 {
        let widths = OverviewWidths::new(width, &app);
        let header = line_text(&overview_header(&widths, false));
        if widths.live == 0 {
            assert!(
                !header.contains("live"),
                "no column means no header at {width} cols: {header:?}"
            );
            continue;
        }
        let row = line_text(&render_overview_row(&app, 0, &widths, false, false));
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
#[test]
fn live_column_width_is_monotone_in_inner_width() {
    for max_name in 8..=22 {
        let mut prev = None;
        for total in 30..=200 {
            let width = live_column_width(max_name, total);
            if let Some(prev_w) = prev
                && width != prev_w
            {
                assert_eq!(
                    (prev_w, width),
                    (0, LIVE_W),
                    "live column must only appear (0 -> LIVE_W), never drop or \
                     flip, at name {max_name}, total {total} ({prev_w} -> {width})",
                );
            }
            prev = Some(width);
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

/// DeepSeek accounts carry a USD balance where OAuth profiles carry a 5h
/// utilization window. When one is on the overview the column header names
/// both roles so the balance cell below it does not read as a mislabeled `%`.
#[test]
fn header_reads_5h_balance_when_a_deepseek_profile_is_present() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = App::new(config_with(
        vec![
            profile("main", 95.0, 10.0, 3600),
            deepseek_profile("ds", &["1.71 USD"]),
        ],
        Some("main"),
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
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
    let app = App::new(config_with(
        vec![profile("main", 95.0, 10.0, 3600)],
        Some("main"),
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
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
    let app = App::new(config_with(
        vec![
            profile("main", 95.0, 10.0, 3600),
            deepseek_profile("ds", &["1.71 USD"]),
        ],
        Some("main"),
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 1, &widths, false, false);
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
    let mut ds = deepseek_profile("ds", &[]);
    ds.third_party_usage = Some(stats);
    let app = App::new(config_with(vec![ds], Some("ds"), vec![]));

    let widths = OverviewWidths::new(120, &app);
    assert_eq!(
        widths.deepseek_amount_w, 5,
        "the legacy row still sizes the amount column (from `31.45`), not 0",
    );
    let row = render_overview_row(&app, 0, &widths, false, false);
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
    let app = App::new(config_with(
        vec![deepseek_profile("ds", &[])],
        Some("ds"),
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);
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
    let app = App::new(config_with(
        vec![
            deepseek_profile("short", &["1.71 USD"]),
            deepseek_profile("long", &["197.50 CNY"]),
        ],
        Some("short"),
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let short_row = render_overview_row(&app, 0, &widths, false, false);
    let long_row = render_overview_row(&app, 1, &widths, false, false);
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
    let app = App::new(config_with(
        vec![deepseek_profile("ds", &["1.71 USD", "100.00 CNY"])],
        None,
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);
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

/// When only one currency is above 0, only that one shows — the zero-balance
/// currency is dropped.
#[test]
fn deepseek_multi_currency_only_shows_above_zero() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = App::new(config_with(
        vec![deepseek_profile("ds", &["0.00 USD", "100.00 CNY"])],
        None,
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);
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
    let app = App::new(config_with(
        vec![deepseek_profile_from_cache(
            "tw",
            crate::testutil::CAPTURED_TWO_WALLET_DS_CACHE,
        )],
        None,
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);
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
    let app = App::new(config_with(
        vec![deepseek_profile_from_cache(
            "one",
            crate::testutil::CAPTURED_ONE_WALLET_DS_CACHE,
        )],
        None,
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);
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
    let app = App::new(config_with(
        vec![deepseek_profile("ds", &["0.00 USD", "0.00 CNY"])],
        None,
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    let row = render_overview_row(&app, 0, &widths, false, false);
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
    let app = App::new(config_with(
        vec![
            deepseek_profile("multi", &["1.71 USD", "197.50 CNY"]),
            deepseek_profile("single", &["3.14 USD"]),
        ],
        None,
        vec![],
    ));
    let widths = OverviewWidths::new(120, &app);
    assert_eq!(
        widths.deepseek_amount_w, 6,
        "amount_w must be 6 (from '197.50'), not capped at 4 or 5"
    );

    let multi_row = render_overview_row(&app, 0, &widths, false, false);
    let single_row = render_overview_row(&app, 1, &widths, false, false);
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
    let mut p = profile("aaaaaaaaaaaa", 40.0, 60.0, 3 * 3600 + 1800);
    if let Some(ref mut usage) = p.usage {
        usage.seven_day = Some(UsageWindow {
            utilization: 60.0,
            resets_at: Some(reset_in(4 * 86400 + 43200)),
        });
    }
    let mut config = config_with(vec![p], None, vec![]);
    config.state.reset_display = Some(crate::profile::ResetDisplay::Both);
    let app = App::new(config);

    let mut five_stamp = Vec::with_capacity((170 - 40 + 1) as usize);
    let mut seven_stamp = Vec::with_capacity((170 - 40 + 1) as usize);
    let mut seven_bar = Vec::with_capacity((170 - 40 + 1) as usize);

    for terminal in 40u16..=170 {
        let widths = OverviewWidths::new(terminal - 4, &app);
        let row = render_overview_row(&app, 0, &widths, false, false);
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
