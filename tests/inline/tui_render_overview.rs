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
    let p = third_party_profile(60.0, 30.0);
    let config = config_with(vec![p], None, vec![]);
    let app = App::new(config);
    let profile = &app.config().profiles[0];
    let (five, seven) = overview_windows(profile);
    let five = five.expect("5h bar synthesizes a window");
    let seven = seven.expect("7d bar synthesizes a window");

    assert!(
        app.active_burn_rate("tp", &UsageInfo::default()).is_none(),
        "no burn history exists for an api-key profile — avg pace is the only source",
    );
    let five_rate = drain_rate(&app, "tp", profile, LABEL_5H, &five)
        .expect("a half-elapsed 5h window yields an avg pace");
    let seven_rate = drain_rate(&app, "tp", profile, LABEL_7D, &seven)
        .expect("a half-elapsed 7d window yields an avg pace");
    assert!(five_rate > 0.0 && seven_rate > 0.0);

    // 60% over the 2.5h elapsed half of a 5h window = 24 %/h.
    assert!(
        (five_rate - 24.0).abs() < 0.5,
        "5h rate in %/h: {five_rate}"
    );
    // 30% over the 3.5d elapsed half of a 7d window ≈ 8.57 %/d.
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
    let a = profile("a", 95.0, 60.0, 9_000);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let p = &app.config().profiles[0];
    let w = p.usage.as_ref().unwrap().five_hour.clone().unwrap();
    assert!(
        drain_rate(&app, "a", p, LABEL_5H, &w).is_none(),
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
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: "tp".into(),
        base_url: Some("https://api.example.com".into()),
        api_key: Some("k".into()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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

/// A chain-eligible OAuth profile with a live 5h window at `util`%, resetting
/// in `reset_secs`.
fn profile(name: &str, threshold: f64, util: f64, reset_secs: i64) -> Profile {
    Profile {
        name: name.into(),
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(threshold),
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let a = profile("a", 95.0, 10.0, 3600);
    let mut config = config_with(vec![a], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true);
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
    let text = line_text(&line);
    assert!(text.contains('×'), "broken login renders ×: {text}");
    assert!(!text.contains('!'), "bell yields to ×: {text}");
    assert!(!text.contains('●'), "active dot yields to ×: {text}");
    let marker = line.spans.iter().find(|s| s.content == "×").unwrap();
    assert_eq!(marker.style.fg, theme::danger().fg);
}

#[test]
fn bell_marker_shows_when_login_is_fine() {
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], None, vec![]);
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true);
    let widths = OverviewWidths::new(80, &app, false);
    let text = line_text(&render_overview_row(&app, 0, &widths, false, true, None));
    assert!(text.contains('!'), "{text}");
    assert!(!text.contains('×'), "{text}");
}

/// A dead / mis-filled long-lived token (⊘) outranks bell (!) and active (●):
/// the next switch would sign sessions out, so it beats a usage alert.
#[test]
fn token_danger_marker_outranks_bell_and_active() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec![]); // active
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true); // bell also fired
    app.session_tokens
        .insert("a".into(), crate::claude::SessionTokenStatus::NotLongLived);
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let a = profile("a", 95.0, 10.0, 3600);
    let mut config = config_with(vec![a], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.session_tokens
        .insert("a".into(), crate::claude::SessionTokenStatus::NotLongLived);
    let widths = OverviewWidths::new(80, &app, false);
    let text = line_text(&render_overview_row(&app, 0, &widths, false, true, None));
    assert!(text.contains('×'), "broken login wins: {text}");
    assert!(!text.contains('⊘'), "token marker yields to ×: {text}");
}

/// A live long-lived token tags the type column (·token) and raises no marker;
/// an expired one raises the ⊘ danger marker.
#[test]
fn long_lived_token_tags_type_column_and_expired_marks() {
    use crate::claude::SessionTokenStatus as S;
    let day = 86_400_000_i64;
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], None, vec![]);
    let mut app = App::new(config);
    // Wide terminal so the type column isn't clamped narrow enough to drop the tag.
    let widths = OverviewWidths::new(120, &app, false);

    app.session_tokens
        .insert("a".into(), S::LongLived(Some(now_ms() as i64 + 340 * day)));
    let live = line_text(&render_overview_row(&app, 0, &widths, false, true, None));
    assert!(
        live.contains("·token"),
        "type column tags token mode: {live}"
    );
    assert!(
        !live.contains('⊘'),
        "a live token raises no danger marker: {live}"
    );

    app.session_tokens
        .insert("a".into(), S::LongLived(Some(now_ms() as i64 - day)));
    let dead = line_text(&render_overview_row(&app, 0, &widths, false, true, None));
    assert!(dead.contains('⊘'), "expired token raises ⊘: {dead}");
}

/// The stale-data cue lives on the refresh countdown now — an underlined name
/// would double-signal, and the bar brackets stay plain dim.
#[test]
fn cached_row_colors_countdown_amber_and_underlines_nothing() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.fetch_status = Some(FetchStatus::Cached);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert("a".to_string(), now_ms() + 30_000);
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.fetch_status = Some(FetchStatus::Failed);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert("a".to_string(), now_ms() + 30_000);
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let config = config_with(vec![third_party_profile(60.0, 30.0)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app, false);
    assert!(
        widths.five_hour >= 26 && widths.seven_day >= 26,
        "test needs both columns wide enough to render a (reset) suffix",
    );
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true, None));
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
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("a", 95.0, 60.0, 9_000);
    a.usage.as_mut().unwrap().seven_day = Some(UsageWindow {
        utilization: 30.0,
        resets_at: Some(reset_in(7 * 86_400 / 2)),
    });
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app, false);
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true, None));
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
    let a = profile("ax-main", 95.0, 10.0, 3600);
    let b = profile("ax-backup", 95.0, 20.0, 3600);
    let config = config_with(vec![a, b], Some("ax-main"), vec![]);
    let app = App::new(config);
    for width in 34u16..=200 {
        let w = OverviewWidths::new(width, &app, false);
        let min =
            fixed_overview_width(w.name, w.kind, w.five_hour, w.seven_day, w.route, 2) + TIMER_SLOT;
        if min > width as usize {
            // Below this the shrink loop has already bottomed out and the row
            // deliberately overflows-and-clips; gap widening isn't the cause.
            continue;
        }
        let used = fixed_overview_width(w.name, w.kind, w.five_hour, w.seven_day, w.route, w.gap)
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
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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
    let a = credentialed_profile("acct", "enterprise");
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(60, &app, false);
    assert_eq!(
        widths.kind, 6,
        "test assumes a 6-wide kind column at this pane width"
    );

    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let widths = OverviewWidths::new(80, &app, false);

    let disabled_line = render_overview_row(&app, 0, &widths, false, true, None);
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

    let enabled_line = render_overview_row(&app, 1, &widths, false, true, None);
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
    let widths = OverviewWidths::new(110, &app, false);

    let non_dim = |line: &Line<'static>| -> Vec<String> {
        line.spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .filter(|s| s.style.fg.is_some() && s.style.fg != theme::dim().fg)
            .map(|s| s.content.to_string())
            .collect()
    };

    let disabled_line = render_overview_row(&app, 0, &widths, false, true, None);
    assert_eq!(
        non_dim(&disabled_line),
        Vec::<String>::new(),
        "every colored span on a disabled row must flatten to dim"
    );

    // The control row must still carry hue, or the assertion above is vacuous.
    let enabled_line = render_overview_row(&app, 1, &widths, false, true, None);
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

    // `pulse_name_spans` keys off `app.started_at.elapsed()`, so two Apps
    // constructed a real interval apart sample different phases of the wave.
    // 450ms is the crest of the 900ms sweep, where the envelope peaks: the
    // phase furthest from the flat 0ms frame, so a surviving pulse shows up at
    // its widest rather than at some near-zero lean that rounds back to base.
    let snapshot = |idx: usize| -> Vec<(String, Option<ratatui::style::Color>)> {
        let mut app = App::new(config.clone());
        app.started_at =
            std::time::Instant::now() - std::time::Duration::from_millis(450 * idx as u64);
        let widths = OverviewWidths::new(110, &app, false);
        render_overview_row(&app, 0, &widths, false, true, None)
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
        app.started_at = std::time::Instant::now() - std::time::Duration::from_millis(elapsed_ms);
        let widths = OverviewWidths::new(110, &app, false);
        render_overview_row(&app, 1, &widths, false, true, None)
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

/// A disabled account is never polled, so its refresh countdown would tick to
/// zero and then claim a refresh forever. The slot renders blank — at full
/// width, so no column downstream shifts.
#[test]
fn disabled_row_blanks_the_refresh_countdown_at_full_width() {
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
    let widths = OverviewWidths::new(110, &app, false);

    let disabled_line = render_overview_row(&app, 0, &widths, false, true, None);
    let enabled_line = render_overview_row(&app, 1, &widths, false, true, None);
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
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let stale = profile("a", 95.0, 73.0, -60); // reset 60s in the past
    let live = profile("b", 95.0, 73.0, 3600); // reset in 1h
    let config = config_with(vec![stale, live], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, false);

    let stale_line = render_overview_row(&app, 0, &widths, false, true, None);
    let live_line = render_overview_row(&app, 1, &widths, false, true, None);
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
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut a = profile("a", 95.0, 73.0, -60); // disabled + past-reset
    a.disabled = true;
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(110, &app, false);

    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec!["a"]);
    let app = App::new(config);
    let cfg = app.config();
    let row = chain_row(&cfg, "a", 0, 0, 8, GAUGE_W, 3, None, Some(7200));
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

/// Every trailer in the panel lands in ONE column, and that column tracks the
/// widest row's content rather than the panel width. The regression this pins:
/// padding each row out to `width` stranded the markers at the far right edge of
/// a wide panel, cells away from the data they mark.
#[test]
fn fallback_panel_parks_trailers_next_to_the_content() {
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
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec!["a"]);
    let app = App::new(config);
    let cfg = app.config();
    let row = chain_row(
        &cfg,
        "a",
        0,
        0,
        8,
        GAUGE_W,
        3,
        Some(BlockedReason::AuthBroken),
        Some(7200),
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
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], Some("a"), vec!["a"]);
    let app = App::new(config);
    let cfg = app.config();

    let build = || {
        chain_row(
            &cfg,
            "a",
            0,
            0,
            8,
            GAUGE_W,
            3,
            Some(BlockedReason::AuthBroken),
            Some(7200),
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

// ── Fork-only tests (codex engine, RESCUE, CLA-FEED, forecast, email column) ──

/// Two-profile roster used by every test in this section: an OAuth profile
/// and an api-key profile (index 1), so placeholder gating is exercised on
/// both row kinds.
fn email_fixture() -> App {
    let oauth = profile("ax-main", 95.0, 10.0, 3600);
    let mut api = profile("relay", 95.0, 0.0, 3600);
    api.base_url = Some("https://api.example.com".into());
    api.api_key = Some("sk-test".into());
    api.usage = None;
    let config = config_with(vec![oauth, api], Some("ax-main"), vec![]);
    App::new(config)
}

/// Layout invariants across widths. The 5-tuple equality is a regression
/// tripwire (today's implementation computes the upstream columns before it
/// reads `has_email`, so it cannot fail; it exists to catch a refactor that
/// lets the carve feed back into column sizing). The live protections are the
/// no-clip bound — REAL row content including the TIMER_SLOT that
/// `fixed_overview_width` omits — checked against BOTH the width model and
/// the actually-rendered line, and gap parity on ungranted layouts.
#[test]
fn email_column_never_disturbs_the_upstream_columns() {
    let app = email_fixture();
    let long = "a-very-long-account-email@example-domain.com";
    for width in [30u16, 48, 53, 58, 64, 81, 93, 102, 110, 124, 140, 200] {
        let plain = OverviewWidths::new(width, &app, false);
        let with = OverviewWidths::new(width, &app, true);
        assert_eq!(plain.account, 0, "no emails → no column at {width}");
        assert_eq!(
            (
                plain.name,
                plain.kind,
                plain.five_hour,
                plain.seven_day,
                plain.route
            ),
            (
                with.name,
                with.kind,
                with.five_hour,
                with.seven_day,
                with.route
            ),
            "regression tripwire: carve fed back into column sizing at {width}"
        );
        if with.account > 0 {
            let used = fixed_overview_width(
                with.name,
                with.kind,
                with.five_hour,
                with.seven_day,
                with.route,
                with.gap,
            ) + TIMER_SLOT
                + ACCOUNT_GAP
                + with.account;
            assert!(
                used <= width as usize,
                "granted layout clips the 5h column at width {width}: used {used}"
            );
            // Model ↔ renderer: the actually-rendered row must fit too, with
            // the widest possible email in the cell.
            let rendered = line_text(&render_overview_row(
                &app,
                0,
                &with,
                false,
                true,
                Some(long),
            ));
            assert!(
                rendered.chars().count() <= width as usize,
                "rendered row overflows at width {width}: {rendered:?}"
            );
        } else {
            // No column → the gap must not depend on has_email at all.
            assert_eq!(plain.gap, with.gap, "gap drifted with no column at {width}");
        }
        // Gap widening works from real spare in BOTH branches: whenever the
        // columns fit at all (upstream deliberately overflows-and-clips below
        // ~33 cols), the widened layout must still fit — the upstream bug was
        // gap widening from the TIMER_SLOT-undercounted figure clipping the
        // 5h column's `%` at narrow widths.
        let plain_min = fixed_overview_width(
            plain.name,
            plain.kind,
            plain.five_hour,
            plain.seven_day,
            plain.route,
            2,
        ) + TIMER_SLOT;
        if plain_min <= width as usize {
            let plain_used = fixed_overview_width(
                plain.name,
                plain.kind,
                plain.five_hour,
                plain.seven_day,
                plain.route,
                plain.gap,
            ) + TIMER_SLOT;
            assert!(
                plain_used <= width as usize,
                "gap widening overflows the plain layout at width {width}: used {plain_used}"
            );
        }
    }
}

/// The exact grant boundary and the cap. For this roster (max name 7 →
/// clamped to the 8 floor; narrow bands: kind 6 / 5h 12 / no 7d / no route)
/// the real row costs `base 33 + TIMER_SLOT 5`, so the column needs
/// `38 + ACCOUNT_GAP + ACCOUNT_MIN = 52` columns: one short of that gets
/// nothing, 52 gets exactly ACCOUNT_MIN. A very wide terminal caps the column
/// at ACCOUNT_MAX and flows the excess into the elastic gaps (clamped at 8).
#[test]
fn email_column_grant_boundary_and_cap() {
    let app = email_fixture();
    assert_eq!(OverviewWidths::new(51, &app, true).account, 0);
    assert_eq!(OverviewWidths::new(52, &app, true).account, ACCOUNT_MIN);
    let wide = OverviewWidths::new(300, &app, true);
    assert_eq!(wide.account, ACCOUNT_MAX);
    assert_eq!(wide.gap, 8, "excess spare beyond the cap widens gaps");
}

/// Cell semantics, pinned via em-dash DELTAS against the no-column layout of
/// the same row (the route and 7d columns legitimately render their own
/// em-dashes, so a bare `contains('—')` would be tautological):
/// - OAuth + cached email → the address renders (truncated to the column).
/// - OAuth + no email → exactly ONE extra em-dash (the pending placeholder).
/// - api-key profile → blank cell, ZERO extra em-dashes (not applicable is
///   not the same as pending — every other surface omits the field).
/// - column not granted → no cell at all.
#[test]
fn email_cell_semantics_by_profile_kind() {
    let app = email_fixture();
    let granted = OverviewWidths::new(160, &app, true);
    let plain = OverviewWidths::new(160, &app, false);
    assert!(granted.account >= ACCOUNT_MIN);

    let header = line_text(&overview_header(&granted));
    assert!(
        header.contains("email"),
        "header names the column: {header}"
    );

    let row = |widths: &OverviewWidths, idx: usize, email: Option<&str>| {
        line_text(&render_overview_row(&app, idx, widths, false, true, email))
    };
    let dashes = |s: &str| s.matches('—').count();

    // OAuth with an email: the address renders truncated, no added em-dash.
    let long = "a-very-long-account-email@example-domain.com";
    let with = row(&granted, 0, Some(long));
    let shown: String = long.chars().take(granted.account - 1).collect();
    assert!(
        with.contains(&format!("{shown}…")),
        "long email truncates with an ellipsis: {with}"
    );
    assert_eq!(dashes(&with), dashes(&row(&plain, 0, None)));

    // OAuth, anchor not seeded: exactly one extra em-dash — the placeholder.
    assert_eq!(
        dashes(&row(&granted, 0, None)),
        dashes(&row(&plain, 0, None)) + 1,
        "unseeded OAuth row must carry the pending placeholder"
    );

    // Api-key profile: blank cell — no placeholder for a profile kind that
    // categorically has no account email.
    assert_eq!(
        dashes(&row(&granted, 1, None)),
        dashes(&row(&plain, 1, None)),
        "api-key row must not render a placeholder"
    );

    // Column not granted (too narrow for even ACCOUNT_MIN) → no cell at all.
    let narrow = OverviewWidths::new(40, &app, true);
    assert_eq!(narrow.account, 0);
    assert!(
        !row(&narrow, 0, Some(long)).contains('@'),
        "no email cell without the column"
    );
}

// ── overview row state cues ──────────────────────────────────────────────

// CDX-2 acceptance: a codex profile with published passive usage renders the
// harness tag, the codex-slot active dot, and real usage bars — asserted on
// the rendered line, not eyeballed.
#[test]
fn codex_row_renders_harness_tag_and_usage_bars() {
    let mut cdx = profile("cdx-a", 95.0, 62.0, 3600);
    cdx.harness = crate::profile::Harness::Codex;
    let mut config = config_with(vec![cdx], None, vec![]);
    config.state.active_codex_profile = Some("cdx-a".into());
    let app = App::new(config);
    let widths = OverviewWidths::new(100, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
    let text = line_text(&line);
    assert!(text.contains("Codex"), "harness tag renders: {text}");
    assert!(text.contains('█'), "usage bar renders: {text}");
    assert!(text.contains('●'), "codex-slot active dot renders: {text}");
    assert!(text.contains("62"), "utilization figure renders: {text}");
}

// ── fallback chain panel: auto-sizing + row trailers ─────────────────────
