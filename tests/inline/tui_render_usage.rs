use super::*;

/// Build a `ReadingsSnapshot` seed the render helpers read, keyed by profile
/// name.
fn snap_with_usage(
    mut s: ReadingsSnapshot,
    name: &str,
    u: crate::usage::UsageInfo,
) -> ReadingsSnapshot {
    s.usage.insert(name.to_string(), u);
    s
}
/// The raw OAuth store entry, as the snapshot builder keeps it. An OAuth-typed
/// name gets the same entry here and (through the picker) in `usage`; a hybrid
/// keeps its provider reading in `usage`, and only this map says what its own
/// plan is.
fn snap_with_oauth_usage(
    mut s: ReadingsSnapshot,
    name: &str,
    u: crate::usage::UsageInfo,
) -> ReadingsSnapshot {
    s.oauth.insert(name.to_string(), u);
    s
}
fn snap_with_status(mut s: ReadingsSnapshot, name: &str, st: FetchStatus) -> ReadingsSnapshot {
    s.fetch_status.insert(name.to_string(), st);
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
fn snap_with_stale(mut s: ReadingsSnapshot, name: &str) -> ReadingsSnapshot {
    s.stale.insert(name.to_string(), true);
    s
}

#[test]
fn usage_stat_prints_a_rounded_percentage_without_changing_its_measurement() {
    let stat = Stat {
        label: "5h".to_string(),
        pct: Some(1.0000000000000009),
        amount: String::new(),
        color: theme::body(),
        burn_rate: None,
        rate_unit: "h",
        pace_pct: None,
        reset_secs: None,
        trailing: String::new(),
        note: None,
        value: None,
    };
    let lines = stat.render(6, 20, 0, 0);
    assert_eq!(
        lines[0].spans.last().map(|s| s.content.as_ref()),
        Some("1%")
    );
    assert_eq!(stat.pct, Some(1.0000000000000009));
}

/// `bar_spans` overlays the `│` pace marker at its cell without ever changing
/// the bar's total width, whether the marker lands over the filled run (ahead of
/// pace) or the empty run (under pace). An out-of-range column draws no marker.
#[test]
fn bar_spans_places_marker_without_changing_width() {
    let fill = Style::default();
    let total =
        |spans: &[Span<'_>]| -> usize { spans.iter().map(|s| s.content.chars().count()).sum() };
    let marker_col = |spans: &[Span<'_>]| -> Option<usize> {
        let mut col = 0;
        for s in spans {
            if s.content == "│" {
                return Some(col);
            }
            col += s.content.chars().count();
        }
        None
    };

    // No marker requested: plain fill + empty, full width, no glyph.
    let plain = bar_spans(4, 10, fill, None);
    assert_eq!(total(&plain), 10);
    assert_eq!(marker_col(&plain), None);

    // Marker over the empty run (under pace) sits exactly at its column.
    let under = bar_spans(4, 10, fill, Some(7));
    assert_eq!(
        total(&under),
        10,
        "width unchanged with a marker over the empty run"
    );
    assert_eq!(marker_col(&under), Some(7));

    // Marker over the filled run (ahead of pace) — still one glyph, same width.
    let ahead = bar_spans(8, 10, fill, Some(3));
    assert_eq!(
        total(&ahead),
        10,
        "width unchanged with a marker over the filled run"
    );
    assert_eq!(marker_col(&ahead), Some(3));

    // Marker at the fill boundary lands on the first empty cell.
    let boundary = bar_spans(4, 10, fill, Some(4));
    assert_eq!(total(&boundary), 10);
    assert_eq!(marker_col(&boundary), Some(4));

    // Out-of-range column → no marker drawn.
    let oob = bar_spans(4, 10, fill, Some(10));
    assert_eq!(marker_col(&oob), None);
    assert_eq!(total(&oob), 10);
}

/// `stats_from_bars` keeps each bar's API label and source order (no inferred
/// window vocabulary, no reordering), puts absolute `used / total` on the eyebrow
/// `amount` (not the bar-line trailing), and leaves only the reset countdown on
/// the trailing.
#[test]
fn stats_from_bars_keeps_api_labels_and_source_order() {
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        // Far-future reset + absolute amounts, given first → stays first.
        tp_bar(
            "time limit",
            0.0,
            now + 30 * 86_400,
            Some(0.0),
            Some(1000.0),
        ),
        // Short reset, percentage-only → stays second (no reordering).
        tp_bar("tokens limit", 1.0, now + 4 * 3600, None, None),
    ];
    let stats = stats_from_bars(&bars, true, true, ResetFmt::default());
    assert_eq!(stats[0].label, "time limit", "API label kept verbatim");
    assert_eq!(stats[1].label, "tokens limit");

    // Amounts live on the eyebrow now, not the bar-line trailing.
    assert_eq!(stats[0].amount, "0 / 1000");
    assert!(!stats[0].trailing.contains('/'));
    assert!(stats[0].trailing.contains("resets in"));

    // Percentage-only bar: no amount, countdown only.
    assert!(stats[1].amount.is_empty());
    assert!(stats[1].trailing.contains("resets in"));
}

/// Two bars sharing the same API label are NOT renamed — z.ai's pair of token
/// limits both read "tokens limit", in source order.
#[test]
fn stats_from_bars_does_not_rename_duplicate_labels() {
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        tp_bar("tokens limit", 0.0, now + 4 * 3600, None, None),
        tp_bar("tokens limit", 12.0, now + 6 * 86_400, None, None),
    ];
    let stats = stats_from_bars(&bars, true, true, ResetFmt::default());
    assert_eq!(stats[0].label, "tokens limit");
    assert_eq!(stats[1].label, "tokens limit");
    assert_eq!(stats[0].pct, Some(0.0));
    assert_eq!(stats[1].pct, Some(12.0));
}

/// A bar whose label decodes to a window (`5h`/`7d`/`30d`) gets the OAuth window
/// predictions: a window-anchored average pace (sub-day → %/h, `<n>d` → %/d) and
/// the ideal-pace marker. Toggles gate them; non-window labels stay bare.
#[test]
fn stats_from_bars_fills_pace_for_windowed_labels() {
    let approx = |a: Option<f64>, b: f64| a.is_some_and(|v| (v - b).abs() < 0.1);
    let now = crate::usage::now_epoch_secs();
    // All three sit under their ideal line, so the over-pace cap is inert and
    // each reads its plain average.
    // 5h window 4h in (resets in 1h), 20% used → 5 %/h, 80% of the way through.
    // 7d window 3.5d in, 35% used → 10 %/d, half elapsed.
    // 30d window 15d in, 30% used → 2 %/d (proves the 30d duration arm).
    let bars = vec![
        tp_bar("5h", 20.0, now + 3600, None, None),
        tp_bar("7d", 35.0, now + 3 * 86_400 + 43_200, None, None),
        tp_bar("30d", 30.0, now + 15 * 86_400, None, None),
    ];

    let stats = stats_from_bars(&bars, true, true, ResetFmt::default());
    assert_eq!(stats[0].rate_unit, "h");
    assert!(approx(stats[0].burn_rate, 5.0), "5h shows %/h average pace");
    assert!(approx(stats[0].pace_pct, 80.0), "5h ideal-pace marker");
    assert_eq!(stats[1].rate_unit, "d");
    assert!(
        approx(stats[1].burn_rate, 10.0),
        "7d shows %/d average pace"
    );
    assert!(
        approx(stats[2].burn_rate, 2.0),
        "30d window now resolves a pace"
    );

    // Both toggles off → no rate, no marker (matches the OAuth gating).
    let bare = stats_from_bars(&bars, false, false, ResetFmt::default());
    assert!(bare.iter().all(|s| s.burn_rate.is_none()));
    assert!(bare.iter().all(|s| s.pace_pct.is_none()));

    // A label that isn't a `<n>h`/`<n>d` window carries no prediction.
    let other = stats_from_bars(
        &[tp_bar("balance", 50.0, now + 3600, None, None)],
        true,
        true,
        ResetFmt::default(),
    );
    assert!(other[0].burn_rate.is_none() && other[0].pace_pct.is_none());
}

/// A past-reset window's `Stat.color` (driving both the bar fill and the `%`
/// figure in `Stat::render`, see [`Stat::render`]) fades to `theme::faint()`
/// — a frozen pre-reset reading awaiting the next fetch. A sibling bar at the
/// SAME utilization with a future reset proves the difference is staleness,
/// not the percentage — a mutation dropping the fade would still pass a
/// same-value comparison, so the control keeps a real (non-faint) util color
/// to red against.
#[test]
fn stale_window_color_fades_to_faint() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        tp_bar("5h", 73.0, now - 60, None, None), // reset 60s in the past
        tp_bar("5h", 73.0, now + 3600, None, None), // reset in 1h
    ];
    let stats = stats_from_bars(&bars, true, true, ResetFmt::default());
    assert_ne!(
        stats[1].color.fg,
        theme::faint().fg,
        "control: a live window keeps its real util color"
    );
    assert_eq!(
        stats[0].color.fg,
        theme::faint().fg,
        "a past-reset window's color fades"
    );
}

fn tp_bar(
    label: &str,
    pct: f64,
    reset_secs: i64,
    used: Option<f64>,
    total: Option<f64>,
) -> crate::providers::UsageBar {
    crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some(crate::usage::epoch_secs_to_iso(reset_secs)),
        used,
        total,
        inference_quota: false,
    }
}

// ── oauth empty states ────────────────────────────────────────────────────────
//
// The oauth body must not spin "loading" forever: a credential-less profile is
// never fetched (issue #2's permanent "loading"), and a Failed fetch is
// terminal. Only a still-possible fetch may show "loading".

#[test]
fn empty_msg_credless_profile_is_terminal() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    assert_eq!(
        oauth_empty_msg(&profile, None),
        "not logged in, use + login on the setup tab"
    );
}

/// A setup token alone gives `/usage` nothing to read: the body names the
/// probe as the way to a reading. The switch on, it loads until a probe lands,
/// and a refused token is as terminal as a failed fetch. The switch without a
/// token has nothing to probe with, so it still asks for a login.
#[test]
fn empty_msg_follows_the_usage_probe_on_a_setup_token_account() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.has_long_lived_sidecar = true;
    assert_eq!(
        oauth_empty_msg(&profile, None),
        "can't read usage, log in the same account with + login on the setup tab, or turn on its usage probe"
    );
    profile.usage_probe = true;
    assert_eq!(oauth_empty_msg(&profile, None), "loading");
    for status in [FetchStatus::Failed, FetchStatus::AuthExpired] {
        assert_eq!(
            oauth_empty_msg(&profile, Some(status)),
            "no usage available",
            "{status:?}"
        );
    }
    profile.has_long_lived_sidecar = false;
    assert_eq!(
        oauth_empty_msg(&profile, None),
        "not logged in, use + login on the setup tab"
    );
}

/// A session-token sidecar with no OAuth pair is a login clauth cannot read
/// usage through: the empty state says what clauth cannot do and how the same
/// account becomes readable, never "not logged in". Read off the profile a
/// config load puts in memory, the way the tab reads it.
#[test]
fn empty_msg_sidecar_only_profile_names_the_unreadable_usage() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("s");
    crate::profile::save_profile(&crate::testutil::blank_profile(&name)).expect("save profile");
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec![name.clone()],
        ..crate::profile::AppState::default()
    })
    .expect("save app state");
    crate::testutil::write_long_lived_sidecar(&name);
    let config = crate::profile::load_config().expect("load config");
    let profile = config.find(&name).expect("loaded profile");
    assert_eq!(
        oauth_empty_msg(profile, None),
        "can't read usage, log in the same account with + login on the setup tab, or turn on its usage probe"
    );
}

/// An Anthropic api key (stored, or an `[env]` token) with no OAuth pair is a
/// login with no usage windows: nothing to fix, so no fix line.
#[test]
fn empty_msg_key_only_profile_has_no_usage_windows() {
    let mut stored = crate::testutil::blank_profile(&crate::profile::ProfileName::from("k"));
    stored.api_key = Some("sk-test".to_string());
    let mut env_token = crate::testutil::blank_profile(&crate::profile::ProfileName::from("e"));
    env_token
        .env
        .insert("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string());
    for profile in [&stored, &env_token] {
        assert_eq!(
            oauth_empty_msg(profile, None),
            "can't read usage, an api key has no usage windows",
            "{}",
            profile.name
        );
    }
}

/// F3d: a profile holding BOTH a long-lived sidecar and an api key reads the
/// sidecar line — the sidecar branch outranks the key branch, the order ruling
/// 5's three arms spell. Swapping the two branches must red this.
#[test]
fn empty_msg_sidecar_outranks_an_api_key_when_both_hold_a_login() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    profile.api_key = Some("sk-test".to_string());
    profile.has_long_lived_sidecar = true;
    assert_eq!(
        oauth_empty_msg(&profile, None),
        "can't read usage, log in the same account with + login on the setup tab, or turn on its usage probe",
        "the sidecar line outranks the api-key line"
    );
}

#[test]
fn empty_msg_failed_fetch_is_terminal() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Failed);
    assert_eq!(
        oauth_empty_msg(&profile, readings.status_of(&profile.name)),
        "no usage available"
    );
}

#[test]
fn empty_msg_pending_fetch_loads() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    assert_eq!(oauth_empty_msg(&profile, None), "loading");
}

/// A disabled profile is never scheduled (`collect_tokens` skips it), so with no
/// seeded cache a fetch never lands — the body must be terminal, not spin
/// "loading" forever. The sibling `empty_msg_pending_fetch_loads` (identical but
/// enabled → "loading") proves it's the `disabled` flag that flips the outcome.
#[test]
fn empty_msg_disabled_profile_is_terminal() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    profile.disabled = true;
    assert_eq!(oauth_empty_msg(&profile, None), "no usage available");
}

/// The third-party body has the same never-scheduled hole: a disabled api-key
/// profile is dropped by `collect_third_party_entries`, so it never loads and
/// must read "no usage available" instead of spinning "loading" forever.
#[test]
fn tp_rows_disabled_profile_is_terminal() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.disabled = true;
    // No `third_party_usage`, no fetch_status → the un-fixed path returns "loading".
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &ReadingsSnapshot::default(),
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert!(
        rendered.iter().any(|l| l.contains("no usage available")),
        "disabled tp body is terminal, got {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|l| l.contains("loading")),
        "disabled tp body must not spin loading, got {rendered:?}"
    );
}

/// The wallet-burn rate rides the funded wallet's balance row — the wallet
/// sibling of the window bars' `· rate` eyebrow section. A two-wallet
/// provider lists both rows under the same label, so the match is on
/// (label, currency) and the unfunded row stays bare; a cold series leaves
/// every row exactly as it rendered before.
#[test]
fn tp_rows_append_the_wallet_burn_rate_to_the_balance_row() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider =
        crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "ds",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "USD balance".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "api balance".to_string(),
                    value: "0.00 USD".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "CNY balance".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "api balance".to_string(),
                    value: "63.34 CNY".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let stringify = |lines: &[Line<'static>]| -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect()
    };
    let rate = crate::usage::WalletRate {
        label: "api balance".to_string(),
        currency: "CNY".to_string(),
        amount: 63.34,
        per_day: 4.17,
    };
    let rendered = stringify(&build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        Some(&rate),
    ));
    assert!(
        rendered
            .iter()
            .any(|l| l.contains("63.34 CNY") && l.contains("~4.2 CNY/day")),
        "the rate rides the funded balance row: {rendered:?}"
    );
    assert!(
        !rendered
            .iter()
            .any(|l| l.contains("0.00 USD") && l.contains("/day")),
        "the unfunded same-label row stays bare: {rendered:?}"
    );
    let bare = stringify(&build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    ));
    assert!(
        bare.iter()
            .any(|l| l.contains("63.34 CNY") && !l.contains("/day")),
        "a cold series leaves the balance row bare: {bare:?}"
    );
}

/// The value column of a third-party text row left-pads each figure to the
/// column's widest value so the right edges align (ragged-left, no decimal
/// point rule, no zero padding) — the contract's numeric-column alignment.
#[test]
fn tp_rows_left_pad_balance_values_to_the_widest_value() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider =
        crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "ds",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "CNY balance".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "api balance".to_string(),
                    value: "50.00 CNY".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "granted".to_string(),
                    value: "1132.60 CNY".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "topped up".to_string(),
                    value: "13.34 CNY".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(
        rendered,
        [
            "CNY balance",
            "  api balance    50.00 CNY",
            "  granted      1132.60 CNY",
            "  topped up      13.34 CNY",
        ]
    );
}

/// A figure is every labelled row whose value opens with a digit (or a `-` sign
/// immediately followed by a digit), whether Body or Danger: a word (`valid`) or
/// a no-data glyph (`-`, `—`) is not. The widest figure left-pads every figure,
/// so the right edges align, and words start at the value column unpadded.
#[test]
fn tp_rows_align_figures_across_body_and_danger_and_leave_words_unpadded() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("or"));
    profile.base_url = Some("https://openrouter.ai/api/v1".to_string());
    profile.provider = crate::providers::Provider::from_base_url("https://openrouter.ai/api/v1");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "or",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "credits".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "api balance".to_string(),
                    value: "0.00 USD".to_string(),
                    kind: crate::providers::StatRowKind::Danger,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "used".to_string(),
                    value: "12.34 USD".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "purchased".to_string(),
                    value: "100.00 USD".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "status".to_string(),
                    value: "valid".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(
        rendered,
        [
            "credits",
            "  api balance    0.00 USD",
            "  used          12.34 USD",
            "  purchased    100.00 USD",
            "  status       valid",
        ]
    );
}

/// A Danger figure joins the width measurement too: when the Danger value is
/// the widest in its block (an overdrawn `-1234.56 USD`), every Body figure
/// left-pads to it.
#[test]
fn tp_danger_rows_join_the_figure_measurement_when_widest() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("or"));
    profile.base_url = Some("https://openrouter.ai/api/v1".to_string());
    profile.provider = crate::providers::Provider::from_base_url("https://openrouter.ai/api/v1");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "or",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "credits".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "api balance".to_string(),
                    value: "-1234.56 USD".to_string(),
                    kind: crate::providers::StatRowKind::Danger,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "used".to_string(),
                    value: "12.34 USD".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(
        rendered,
        [
            "credits",
            "  api balance  -1234.56 USD",
            "  used            12.34 USD",
        ]
    );
}

/// Figure-ness rides on the value's own text: one number (optionally signed)
/// with an optional unit/currency word is a figure; a word, a no-data glyph, or
/// a `·`-joined / parenthesized composite is not. Two known residuals are named
/// rather than fixed — `$5` reads as a word (leading currency) and an ISO date
/// as a figure; no current producer emits either shape.
#[test]
fn is_figure_reads_off_the_values_own_text() {
    for (value, want) in [
        ("12 days", true),
        ("52.2M  (451 calls)", false),
        ("5h 40% · 7d 80%", false),
        ("valid", false),
        ("—", false),
        ("-", false),
        ("-3.50 USD", true),
        ("44,456,531", true),
        ("3", true),
        ("$5", false),
        ("2026-10-01", true),
    ] {
        assert_eq!(is_figure(value), want, "{value:?}");
    }
}

/// z.ai's per-model token totals and the `total` row print full grouped integers
/// in the Usage detail (`44,456,531`, not `44.5M`), while every other reader of
/// those rows keeps the compact `value` string.
#[test]
fn zai_usage_detail_prints_full_grouped_counts() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("zai"));
    profile.base_url = Some("https://api.z.ai/anthropic".to_string());
    profile.provider = crate::providers::Provider::from_base_url("https://api.z.ai/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "zai",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "7d tokens".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "GLM-5.2".to_string(),
                    value: "44.5M".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: Some(crate::providers::StatRowRaw::Count { value: 44_456_531 }),
                },
                crate::providers::StatRow {
                    label: "GLM-4.7".to_string(),
                    value: "7.8M".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: Some(crate::providers::StatRowRaw::Count { value: 7_788_526 }),
                },
                crate::providers::StatRow {
                    label: "total".to_string(),
                    value: "52.2M  (451 calls)".to_string(),
                    kind: crate::providers::StatRowKind::Faint,
                    raw: Some(crate::providers::StatRowRaw::Calls {
                        value: 52_245_057,
                        calls: 451,
                    }),
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(
        rendered,
        [
            "7d tokens",
            "  GLM-5.2      44,456,531",
            "  GLM-4.7       7,788,526",
            "  total        52,245,057  (451 calls)",
        ]
    );
}

/// The `total` row's leading number joins the figure column like any figure:
/// when it is wider than every model figure, the model figures left-pad to it
/// and the `(N calls)` suffix trails outside the column.
#[test]
fn zai_total_joins_the_figure_column_when_wider_than_models() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("zai"));
    profile.base_url = Some("https://api.z.ai/anthropic".to_string());
    profile.provider = crate::providers::Provider::from_base_url("https://api.z.ai/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "zai",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "7d tokens".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "GLM-5.2".to_string(),
                    value: "9M".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: Some(crate::providers::StatRowRaw::Count { value: 9_000_000 }),
                },
                crate::providers::StatRow {
                    label: "GLM-4.7".to_string(),
                    value: "8M".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: Some(crate::providers::StatRowRaw::Count { value: 8_000_000 }),
                },
                crate::providers::StatRow {
                    label: "total".to_string(),
                    value: "11M  (451 calls)".to_string(),
                    kind: crate::providers::StatRowKind::Faint,
                    raw: Some(crate::providers::StatRowRaw::Calls {
                        value: 11_000_000,
                        calls: 451,
                    }),
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(
        rendered,
        [
            "7d tokens",
            "  GLM-5.2       9,000,000",
            "  GLM-4.7       8,000,000",
            "  total        11,000,000  (451 calls)",
        ]
    );
}

/// A z.ai cache written before this build carries no `raw`: its rows keep the
/// compact `value` string (`44.5M`), never hidden and never refetched — the
/// decided transitional behaviour until a successful refetch.
#[test]
fn zai_old_cache_row_renders_its_compact_value() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("zai"));
    profile.base_url = Some("https://api.z.ai/anthropic".to_string());
    profile.provider = crate::providers::Provider::from_base_url("https://api.z.ai/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "zai",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "7d tokens".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "GLM-5.2".to_string(),
                    value: "44.5M".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: None,
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(rendered, ["7d tokens", "  GLM-5.2      44.5M"]);
}

/// A per-tool quota row (z.ai's `quota_stats` breakdown) drives through the
/// Usage detail and renders its raw count full grouped, not the compact `1.2k`
/// the `value` string carries.
#[test]
fn zai_per_tool_row_renders_its_full_grouped_count() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("zai"));
    profile.base_url = Some("https://api.z.ai/anthropic".to_string());
    profile.provider = crate::providers::Provider::from_base_url("https://api.z.ai/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "zai",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![
                crate::providers::StatRow {
                    label: "mcp tools".to_string(),
                    value: String::new(),
                    kind: crate::providers::StatRowKind::Heading,
                    raw: None,
                },
                crate::providers::StatRow {
                    label: "search-prime".to_string(),
                    value: "1.2k".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: Some(crate::providers::StatRowRaw::Count { value: 1_234 }),
                },
                crate::providers::StatRow {
                    label: "web-reader".to_string(),
                    value: "3".to_string(),
                    kind: crate::providers::StatRowKind::Body,
                    raw: Some(crate::providers::StatRowRaw::Count { value: 3 }),
                },
            ],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert_eq!(
        rendered,
        ["mcp tools", "  search-prime  1,234", "  web-reader       3",]
    );
}

/// The Usage tab is where an operator decides whether an account can still
/// serve, so a provider's refusal renders BESIDE the figures it qualifies
/// rather than in place of them. Discarding the wallets left the tab saying
/// only that something was wrong, with no way to see how short the account was.
#[test]
fn tp_rows_render_a_refusal_under_the_figures_it_qualifies() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider =
        crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "ds",
        serde_json::from_str(crate::testutil::DEEPSEEK_UNFUNDED_CACHE_BYTES)
            .expect("the unfunded balance cache parses"),
    );
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert!(
        rendered.iter().any(|l| l.contains("0.00 CNY")),
        "the wallet figure must survive the refusal, got {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|l| l.contains(crate::providers::LOW_BALANCE)),
        "the refusal must render, got {rendered:?}"
    );
}

/// A recognised-provider profile with NO credential its provider can use is
/// never scheduled either, so it has the same hole. Reachable from the shipped
/// TUI: a blank Setup key field stores `None`.
#[test]
fn tp_rows_uncredentialed_profile_is_terminal() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider =
        crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    profile.api_key = None;
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &ReadingsSnapshot::default(),
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert!(
        !rendered.iter().any(|l| l.contains("loading")),
        "a profile no leg will fetch must not spin loading, got {rendered:?}"
    );
    assert!(
        rendered.iter().any(|l| l.contains("no api key")),
        "and it must name the fix, got {rendered:?}"
    );
}

/// An EMPTY or whitespace-only api key is the same never-scheduled state: the
/// render layer must read it through the shared credential test and say so
/// instead of spinning "loading" for a fetch no leg will run.
#[test]
fn tp_rows_empty_key_profile_is_terminal() {
    for key in [String::new(), "  \t ".to_string()] {
        let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
        profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
        profile.provider =
            crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
        profile.api_key = Some(key);
        let rendered: Vec<String> = build_tp_rows(
            &profile,
            52,
            &ReadingsSnapshot::default(),
            false,
            false,
            ResetFmt::default(),
            None,
        )
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .collect();
        assert!(
            !rendered.iter().any(|l| l.contains("loading")),
            "a profile no leg will fetch must not spin loading, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("no api key")),
            "and it must name the fix, got {rendered:?}"
        );
    }
}

/// The three dead-credential causes read as three messages: a lapsed Alibaba
/// session, a never-captured one, and a rejected api key. A non-Alibaba
/// profile has no session, so its verdict can only ever mean the third.
#[test]
fn tp_rows_auth_expired_copy_splits_by_credential() {
    let body = |profile: &super::Profile, readings: &ReadingsSnapshot| -> String {
        build_tp_rows(
            profile,
            52,
            readings,
            false,
            false,
            ResetFmt::default(),
            None,
        )
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
        .collect::<Vec<_>>()
        .join("")
    };

    // A dead key on a typed api-key provider.
    let mut ds = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    ds.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    ds.provider = crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    ds.api_key = Some("sk-dead".to_string());
    let mut readings =
        snap_with_status(ReadingsSnapshot::default(), "ds", FetchStatus::AuthExpired);
    let msg = body(&ds, &readings);
    assert!(msg.contains("api key rejected"), "got {msg}");

    // A dead key on an unrecognised endpoint reaches the same verdict.
    let mut generic = crate::testutil::blank_profile(&crate::profile::ProfileName::from("generic"));
    generic.base_url = Some("https://proxy.example/v1".to_string());
    generic.api_key = Some("sk-dead".to_string());
    readings = snap_with_status(readings, "generic", FetchStatus::AuthExpired);
    let msg = body(&generic, &readings);
    assert!(msg.contains("api key rejected"), "got {msg}");
    assert!(
        !msg.contains("console"),
        "no session exists to name, got {msg}"
    );

    // An Alibaba profile with a stored session: the session lapsed.
    let mut qwen = crate::testutil::blank_profile(&crate::profile::ProfileName::from("qwen"));
    qwen.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    qwen.provider = crate::providers::Provider::from_base_url(
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
    );
    qwen.console = Some(crate::profile::ConsoleCredential {
        token: "dead".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    readings = snap_with_status(readings, "qwen", FetchStatus::AuthExpired);
    let msg = body(&qwen, &readings);
    assert!(msg.contains("console login expired"), "got {msg}");

    // The same account before any session was captured: it needs one.
    qwen.console = None;
    let msg = body(&qwen, &readings);
    assert!(msg.contains("console login needed"), "got {msg}");
    assert!(!msg.contains("expired"), "nothing lapsed, got {msg}");
}

/// `TP_KEY_W` exists so every key cell in a stat block has one width and the
/// value column starts at one column. `key_cell` widens rather than truncates,
/// so a label longer than the constant does not clip — it pushes that one row's
/// value right of all its siblings, and the misalignment reads as a render bug
/// rather than as a too-narrow constant. The block that catches it is the one
/// with the longest label, DeepSeek's.
#[test]
fn tp_body_rows_share_one_value_column() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider =
        crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "ds",
        serde_json::from_str(crate::testutil::DEEPSEEK_CACHE_BYTES)
            .expect("the captured balance cache parses"),
    );

    // A body row is `["  " + key_cell, pad?, value]`: the value is left-padded
    // to the column's widest figure, so its key cell is still the fixed-width
    // span before it and every key still starts at one column.
    let starts: Vec<(String, usize)> = build_tp_rows(
        &profile,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .filter(|l| l.spans.len() >= 2)
    .map(|l| {
        (
            l.spans[0].content.trim().to_string(),
            l.spans[0].content.chars().count(),
        )
    })
    .collect();

    assert!(starts.len() >= 3, "the balance block renders: {starts:?}");
    let first = starts[0].1;
    assert!(
        starts.iter().all(|(_, w)| *w == first),
        "every key cell has one width: {starts:?}",
    );
}

/// An Alibaba profile with a console session but NO api key is the inverse: it
/// IS scheduled (its quota runs on the console session), so it must keep
/// loading rather than claim a missing key.
#[test]
fn tp_rows_console_only_alibaba_still_loads() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("qwen"));
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    profile.base_url = Some(base.to_string());
    profile.provider = crate::providers::Provider::from_base_url(base);
    profile.api_key = None;
    profile.console = Some(crate::profile::ConsoleCredential {
        token: "t".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    let rendered: Vec<String> = build_tp_rows(
        &profile,
        52,
        &ReadingsSnapshot::default(),
        false,
        false,
        ResetFmt::default(),
        None,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    assert!(
        rendered.iter().any(|l| l.contains("loading")),
        "a scheduled profile is still loading, got {rendered:?}"
    );
}

/// With no fetched plan, the Usage `plan` row must match the Overview's tier
/// label (`account_tier`) instead of a bare "oauth"/"api", so the two surfaces
/// never disagree. A `subscription_type` claim renders as its tier.
#[test]
fn header_lines_plan_falls_back_to_account_tier() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    // No `usage`, no `third_party_usage` → the plan-label fallback is exercised.
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let plan_row: String = header_lines(&profile, &header, 52, &ReadingsSnapshot::default())
        .first()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .unwrap_or_default();
    let expected = crate::format::account_tier(&profile, None)
        .and_then(|t| t.display())
        .expect("a 'max' token claims a tier");
    assert_eq!(expected, "Claude Max", "sanity: the tier label under test");
    assert!(
        plan_row.contains(&expected),
        "plan row shows the endpoint tier, got {plan_row:?}"
    );
    assert!(
        !plan_row.contains("oauth"),
        "plan row must not fall back to the bare 'oauth' literal, got {plan_row:?}"
    );
}

/// A HYBRID profile — an OAuth pair AND a custom `base_url`, no api key, no
/// recognised provider — renders its FETCHED tier, not the bare "api" literal.
/// `is_oauth()` keys on `base_url`, so a hybrid reads false there while the body
/// this header heads still draws its live OAuth window bars (the shared
/// cache-selector fork this file uses for the body). Reading `api` directly
/// above Anthropic 5h/7d bars sourced from the very `UsageInfo` whose tier was
/// discarded is the disagreement; the header and the body must share one fork.
///
/// Reachable in production two ways, both leaving `credentials` intact:
/// `edit_profile_endpoint` sets `base_url` and clears only `third_party_usage`,
/// and `capture_snapshot` reads the credentials file and the endpoint config
/// independently into one profile. The scheduler then refetches it every tick,
/// because `collect_tokens` keys on `claude_ai_oauth.is_some()`, not `is_oauth()`.
#[test]
fn header_lines_plan_shows_a_hybrid_oauth_profiles_fetched_tier() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("hybrid"));
    profile.base_url = Some("https://proxy.example/anthropic".to_string());
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "hybrid",
        crate::usage::UsageInfo {
            plan: Some(crate::usage::PlanInfo {
                tier: crate::usage::PlanTier::Max(Some(20)),
                plan_display_name: None,
                subscription_status: None,
                codex_plan: None,
                email: None,
            }),
            ..Default::default()
        },
    );
    assert!(
        !profile.is_oauth() && !profile.is_third_party() && profile.api_key.is_none(),
        "fixture must be the hybrid shape, or this pins nothing"
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let plan_row: String = header_lines(&profile, &header, 52, &readings)
        .first()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .unwrap_or_default();
    assert!(
        plan_row.contains("Claude Ultra"),
        "hybrid plan row shows its fetched tier, got {plan_row:?}"
    );
    assert!(
        !plan_row.contains("api"),
        "hybrid plan row must not fall through to the bare 'api' literal, got {plan_row:?}"
    );
}

/// The Usage plan row reads the server's own plan name like every other plan
/// surface; the token stamp and the local map would both say `Claude Max`.
#[test]
fn header_lines_plan_shows_the_server_plan_name() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("named"));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "named",
        crate::usage::UsageInfo {
            plan: Some(crate::usage::PlanInfo {
                tier: crate::usage::PlanTier::Max(Some(5)),
                plan_display_name: Some("Ultra Lite".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::OAuth,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let plan_row: String = header_lines(&profile, &header, 52, &readings)
        .first()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .unwrap_or_default();
    assert!(
        plan_row.contains("Claude Ultra Lite"),
        "the plan row shows the server's name, got {plan_row:?}"
    );
}

/// With no fetched plan AND no tier the token can claim, the `plan` row takes
/// the house no-data dash in the faint no-data treatment. The bare "Claude" it
/// printed before named a plan the account never had.
#[test]
fn header_lines_plan_dashes_when_no_tier_is_known() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    // Credentialed, so the row is a live OAuth account rather than an empty
    // shell — but the token claims nothing clauth can classify.
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("something_new".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let lines = header_lines(&profile, &header, 52, &ReadingsSnapshot::default());
    let plan_line = lines.first().expect("plan row");
    let value = plan_line.spans.last().expect("plan value span");
    assert_eq!(value.content, "—", "plan value, got {:?}", plan_line.spans);
    assert_eq!(
        value.style.fg,
        theme::faint().fg,
        "the no-data dash takes the faint treatment"
    );
    let plan_row: String = plan_line.spans.iter().map(|s| s.content.clone()).collect();
    assert!(
        !plan_row.contains("Claude"),
        "an unfetched plan must not name a tier, got {plan_row:?}"
    );
}

/// The `usage auto-start in …` countdown on the `plan` row, flush right: gray,
/// shown for ANY account with `auto_start` on, queue toggle on or off (owner
/// 2026-09-01 — the text moved off the Fallback card, then right onto the plan
/// row). The value is THIS account's next kick: with a queue slot, the LATER of
/// the queue's next-opening estimate and the account's own 5h window reset —
/// the kick fires once the queue gate has cleared AND this window has lapsed,
/// so either can delay it. Without a slot (toggle off, or the profile is
/// excluded from the queue) it is the account's own reset alone. A kick due on
/// both clocks reads `usage auto-start due now`.
#[test]
fn header_lines_auto_start_kick_text_reads_the_later_of_gate_and_own_reset() {
    let lines_of = |auto_start: bool, reset_in: Option<i64>, slot: Option<QueueSlot>| {
        let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
        profile.auto_start = auto_start;
        let readings = match reset_in.map(|secs| crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 0.0,
                resets_at: Some(crate::testutil::countdown_reset_in(secs)),
            }),
            ..Default::default()
        }) {
            Some(u) => snap_with_usage(ReadingsSnapshot::default(), "a", u),
            None => ReadingsSnapshot::default(),
        };
        let header = HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: slot,
            diag: DiagFlags::default(),
            peak: None,
        };
        header_lines(&profile, &header, 52, &readings)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let slot = |next_in: Option<i64>| {
        Some(QueueSlot {
            position: 1,
            total: 2,
            next_in,
        })
    };
    // `plan` + no-data dash measure 11 cells; a 52-cell row pads the kick text
    // flush right with the house 3-cell minimum gap.
    let row = |kick: &str| {
        format!(
            "plan      —{}{}",
            " ".repeat(52 - 11 - kick.chars().count()),
            kick
        )
    };

    // Gate and own reset tie: either estimate reads the same value.
    assert_eq!(
        lines_of(true, Some(8880), slot(Some(8910)))[0],
        row("usage auto-start in 2h 28m")
    );

    // Own reset LATER than the gate: the window is live past the gate, so the
    // kick waits for the reset — the gate value alone would name an instant no
    // kick fires at.
    assert_eq!(
        lines_of(true, Some(8880), slot(Some(3600)))[0],
        row("usage auto-start in 2h 28m")
    );

    // Gate LATER than the own reset: the window lapsed (or lapses) inside the
    // gap, so the queue gate holds the kick.
    assert_eq!(
        lines_of(true, Some(3600), slot(Some(8880)))[0],
        row("usage auto-start in 2h 28m")
    );

    // Own window already lapsed with the gate still closed: the gate is the
    // estimate.
    assert_eq!(
        lines_of(true, Some(-60), slot(Some(8880)))[0],
        row("usage auto-start in 2h 28m")
    );

    // Gate cleared, window still live: the reset is the estimate.
    assert_eq!(
        lines_of(true, Some(8880), slot(None))[0],
        row("usage auto-start in 2h 28m")
    );

    // Both clocks due: due now.
    assert_eq!(
        lines_of(true, None, slot(None))[0],
        row("usage auto-start due now")
    );

    // No slot — toggle off, or the account is excluded from the queue: the
    // account's own reset is the moment its lapsed-leg kick fires.
    assert_eq!(
        lines_of(true, Some(8880), None)[0],
        row("usage auto-start in 2h 28m")
    );

    // Own window already lapsed: due now.
    assert_eq!(
        lines_of(true, Some(-60), None)[0],
        row("usage auto-start due now")
    );

    // Not opted in: no kick text at all.
    let lines = lines_of(false, Some(8880), None);
    assert!(
        !lines.iter().any(|l| l.contains("kick")),
        "an account without auto_start gets no kick text, got {lines:?}"
    );
}

/// Tight rows: the kick text truncates with a trailing ellipsis, keeping the
/// 3-cell gap, and drops whole when not even a hint fits.
#[test]
fn header_lines_kick_text_truncates_then_drops_on_tight_rows() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.auto_start = true;
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "a",
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 0.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(
                    crate::usage::now_epoch_secs() + 8880,
                )),
            }),
            ..Default::default()
        },
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: Some(QueueSlot {
            position: 1,
            total: 2,
            next_in: Some(8880),
        }),
        diag: DiagFlags::default(),
        peak: None,
    };
    let row = |w: u16| {
        header_lines(&profile, &header, w, &readings)[0]
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect::<String>()
    };

    // 26 cells: 11 left + 3 gap leaves 12, so "usage auto-start in 2h 28m"
    // becomes "usage auto-…" and the 3-cell gap holds.
    assert_eq!(row(26), "plan      —   usage auto-…");

    // 15 cells: the gap alone eats what remains, so the kick text drops whole.
    assert_eq!(row(15), "plan      —");
}

/// A stored key with NO base_url — reachable from a captured `~/.claude`
/// settings.json that carried a key but no endpoint (the pre-helper env
/// residual `read_claude_endpoint_config` still reads), or from a hand-edited
/// config.toml — renders through the OAUTH arm: its figures would live in the
/// OAuth cache, the same answer the shared cache selector gives and the one
/// the profile load seeds `third_party_usage` with. The old site-local
/// spelling (`api_key.is_some() || is_third_party`) rendered the third-party
/// arm over a `third_party_usage` load had seeded `None`, the disagreement
/// this fork keys out.
#[test]
fn a_key_without_an_endpoint_renders_through_the_oauth_arm() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("keyonly"));
    profile.api_key = Some("sk-orphan".to_string());
    assert!(
        profile.api_key.is_some() && !profile.usage_cache_is_third_party(),
        "fixture must be the state the two spellings disagree on",
    );

    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let plan_row: String = header_lines(&profile, &header, 52, &ReadingsSnapshot::default())
        .first()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .unwrap_or_default();
    assert!(
        !plan_row.contains("api"),
        "a key with no endpoint has no third-party arm to head: {plan_row:?}"
    );

    let app = App::new(AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile.clone()],
    });
    let body: String = build_usage_lines(
        &profile,
        52,
        &header,
        &app,
        &ReadingsSnapshot::default(),
        true,
        true,
        ResetFmt::default(),
    )
    .iter()
    .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
    .collect();
    assert!(
        body.contains("can't read usage, an api key has no usage windows"),
        "the body takes the OAuth arm's empty message, got {body:?}"
    );
    // Notes close the pane (cloudy's placement): after the usage blocks, never
    // between header and body.
    let at_notes = body.find("notes").expect("notes row present");
    let at_empty = body
        .find("can't read usage, an api key has no usage windows")
        .expect("empty message present");
    assert!(
        at_notes > at_empty,
        "notes render below the usage blocks, got {body:?}"
    );
}

/// The `account_tier` fallback is gated to OAuth profiles: an api-key profile
/// with no plan keeps "api", never a leaked tier guess or its raw endpoint url.
/// Pins the regression an ungated `account_tier` call would cause on
/// DeepSeek/z.ai/generic rows.
#[test]
fn header_lines_plan_keeps_api_for_api_key_profiles() {
    let profile = crate::profile::Profile::new(
        "a".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let plan_row: String = header_lines(&profile, &header, 52, &ReadingsSnapshot::default())
        .first()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .unwrap_or_default();
    assert!(
        plan_row.contains("api"),
        "api-key plan row stays 'api', got {plan_row:?}"
    );
    assert!(
        !plan_row.contains("deepseek"),
        "must not leak the raw endpoint url into the plan row, got {plan_row:?}"
    );
}

/// The canceled pill is sourced purely from the per-frame snapshot (populated at
/// startup by `bootstrap_fetch`'s on-disk cache seed, see
/// `usage::scheduler::try_seed_cache`), never a live fetch. A profile carrying
/// a prior session's canceled plan must show the pill from the cached state
/// alone, before any network call.
#[test]
fn status_lines_shows_canceled_from_a_prior_sessions_cached_plan() {
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "a",
        UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Free,
                plan_display_name: None,
                subscription_status: Some("canceled".to_string()),
                codex_plan: None,
                email: None,
            }),
            ..Default::default()
        },
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let rendered = text(status_lines(&profile, &header, 120, &readings));
    assert!(rendered.contains("canceled"), "got {rendered:?}");

    // Both halves on one surface: the tier row names the TIER, the status row
    // names the STATUS. Folding "canceled" into the tier is what made the JSON
    // surfaces disagree about the same account, and this is where a reader
    // would first be tempted to do it again.
    let header_text = text(header_lines(&profile, &header, 120, &readings));
    let plan_row = header_text.lines().next().expect("plan row");
    assert!(
        plan_row.contains("Claude Free") && !plan_row.contains("canceled"),
        "the plan row carries the tier alone, got {plan_row:?}"
    );
    assert!(
        header_text.contains("canceled"),
        "the status block below it still says canceled, got {header_text:?}"
    );
}

/// The organization's refusal names itself apart from a canceled plan on the
/// Usage card, its pill and its fix both, pinned whole: swapping either for
/// the canceled pair turns this red. The control: a canceled plan reads its own
/// pair.
#[test]
fn status_lines_name_an_organizations_refusal_apart_from_a_canceled_plan() {
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let rendered = |status: &str| {
        let readings = snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            UsageInfo {
                plan: Some(PlanInfo {
                    tier: PlanTier::Free,
                    plan_display_name: None,
                    subscription_status: Some(status.to_string()),
                    codex_plan: None,
                    email: None,
                }),
                ..Default::default()
            },
        );
        status_text(&status_lines(&profile, &header, 120, &readings))
    };
    assert_eq!(
        rendered(crate::usage::ORG_REFUSED_STATUS),
        "status    [ org refused ]\n└ its organization refuses this account, no login fixes that"
    );
    assert_eq!(
        rendered("canceled"),
        "status    [ canceled ]\n└ this subscription has been canceled",
        "control: a canceled plan reads its own pill and fix"
    );
}

/// Regression guard the other direction: an un-canceled cached plan never
/// paints the canceled pill.
#[test]
fn status_lines_no_canceled_pill_when_subscription_is_active() {
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "a",
        UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Free,
                plan_display_name: None,
                subscription_status: None,
                codex_plan: None,
                email: None,
            }),
            ..Default::default()
        },
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let rendered = text(status_lines(&profile, &header, 120, &readings));
    assert!(!rendered.contains("canceled"), "got {rendered:?}");
}

/// Shared fixture for the two disabled-rung tests: a header whose lower rungs
/// are all armed, so each test's assertion is about what the disabled rung does
/// to them rather than about which rung happened to fire.
fn disabled_rung_header(kick: bool) -> HeaderState {
    use crate::usage::KickBlock;
    HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: kick.then(|| KickBlock {
            streak: 3,
            rejected: true,
            until: Some(now_epoch_secs() + 3600),
            next_retry: now_epoch_secs() + 30,
        }),
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    }
}

fn status_text(ls: &[Line<'_>]) -> String {
    ls.iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `stale` cue fires off the age field alone: a stale-aged cache renders
/// the pill (warning BOLD, like `cached`), a fresh one does not. `fetch_status`
/// stays out of it, so the pin proves the cue is not a second spelling of the
/// fetch outcome.
#[test]
fn status_lines_renders_stale_cue_from_age_alone() {
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };

    let stale = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_stale(ReadingsSnapshot::default(), "a");
    let lines = status_lines(&stale, &header, 120, &readings);
    let stale_spans: Vec<_> = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| s.content == "stale")
        .collect();
    assert_eq!(stale_spans.len(), 1, "one stale pill label");
    assert_eq!(
        stale_spans[0].style,
        theme::warning().add_modifier(Modifier::BOLD),
        "stale pill takes the warning BOLD treatment"
    );

    let fresh = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let fresh_readings = ReadingsSnapshot::default();
    let rendered = status_text(&status_lines(&fresh, &header, 120, &fresh_readings));
    assert!(!rendered.contains("stale"), "got {rendered:?}");
}

/// A `Cached` fetch outcome and a `stale` age cue coexist: the two signals
/// differ, so the cue must not gate on `fetch_status`.
#[test]
fn status_lines_stale_cue_coexists_with_cached_fetch_status() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let readings = snap_with_stale(readings, "a");
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let rendered = status_text(&status_lines(&profile, &header, 120, &readings));
    assert!(rendered.contains("cached"), "got {rendered:?}");
    assert!(rendered.contains("stale"), "got {rendered:?}");
}

/// The stale cue prepends the fetch row — one line, never its own rung, with
/// `stale` left of the fetch pill.
#[test]
fn status_lines_stale_prepends_the_fetch_row() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let readings = snap_with_stale(readings, "a");
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let lines = status_lines(&profile, &header, 120, &readings);
    let merged = lines
        .iter()
        .position(|l| {
            let stale = l.spans.iter().position(|s| s.content == "stale");
            let fetch = l.spans.iter().position(|s| s.content.contains("cached"));
            stale.is_some() && fetch.is_some()
        })
        .map(|i| &lines[i]);
    let Some(merged) = merged else {
        panic!("stale and the fetch pill must share one row: {lines:?}");
    };
    let stale_at = merged
        .spans
        .iter()
        .position(|s| s.content == "stale")
        .expect("stale on the merged row");
    let cached_at = merged
        .spans
        .iter()
        .position(|s| s.content.contains("cached"))
        .expect("cached on the merged row");
    assert!(
        stale_at < cached_at,
        "stale leads the fetch pill: {merged:?}"
    );
    // No second stale rung anywhere: exactly one stale span across all rows.
    let stale_spans = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| s.content == "stale")
        .count();
    assert_eq!(stale_spans, 1, "one stale pill, merged: {lines:?}");
}

/// A keyless third-party profile can never be polled, so the fetch row must
/// not claim `up to date`: it renders the `[ no key ]` pill instead. Without
/// figures the body already names the fix, so the pill carries no hint here.
#[test]
fn status_lines_keyless_third_party_renders_no_key_pill() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.provider = Some(crate::providers::Provider::DeepSeek);
    profile.base_url = Some("https://api.deepseek.com".to_string());
    profile.api_key = None;
    // A historical outcome is not current truth once the key is gone: the
    // pill outranks it.
    let readings = snap_with_stale(
        snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached),
        "a",
    );
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let lines = status_lines(&profile, &header, 120, &readings);
    let merged = lines.iter().position(|l| {
        let stale = l.spans.iter().position(|s| s.content == "stale");
        let key = l.spans.iter().position(|s| s.content == "no key");
        stale.is_some() && key.is_some()
    });
    let Some(i) = merged else {
        panic!("stale and the no-key pill must share one row: {lines:?}");
    };
    let stale_at = lines[i]
        .spans
        .iter()
        .position(|s| s.content == "stale")
        .expect("stale on the merged row");
    let key_at = lines[i]
        .spans
        .iter()
        .position(|s| s.content == "no key")
        .expect("no key on the merged row");
    assert!(stale_at < key_at, "stale leads the pill: {:?}", lines[i]);
    let rendered = status_text(&lines);
    assert!(!rendered.contains("up to date"), "got {rendered:?}");
    assert!(
        !rendered.contains("cached"),
        "a stale outcome does not outrank the missing key: {rendered:?}"
    );
    assert!(
        !rendered.contains("no api key set"),
        "no figures -> the body names the fix, the status block does not: {rendered:?}"
    );
}

/// The pill's gate is both work lists' own membership: a keyless generic
/// endpoint (base url, no provider) renders it too, while a hybrid that keeps
/// its OAuth pair never does — the pair polls usage, so that account has no
/// key to miss. The fix hint rides when figures render.
#[test]
fn status_lines_no_key_gate_is_the_work_lists_membership() {
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let mut generic = crate::testutil::blank_profile(&crate::profile::ProfileName::from("g"));
    generic.base_url = Some("https://example.com".to_string());
    generic.api_key = None;
    let rendered = status_text(&status_lines(
        &generic,
        &header,
        120,
        &ReadingsSnapshot::default(),
    ));
    assert!(rendered.contains("no key"), "got {rendered:?}");
    assert!(!rendered.contains("up to date"), "got {rendered:?}");

    let mut hybrid = crate::testutil::blank_profile(&crate::profile::ProfileName::from("h"));
    hybrid.base_url = Some("https://example.com".to_string());
    hybrid.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let rendered = status_text(&status_lines(
        &hybrid,
        &header,
        120,
        &ReadingsSnapshot::default(),
    ));
    assert!(
        !rendered.contains("no key"),
        "a pair polls this account: {rendered:?}"
    );

    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "g",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    let rendered = status_text(&status_lines(&generic, &header, 120, &readings));
    assert!(
        rendered.contains("no api key set"),
        "figures -> the status hint names the fix: {rendered:?}"
    );
}

/// A keyless proxy account (its key cleared on the setup tab) keeps its last
/// figures, and its fix hint names the re-login that mints a new key, never
/// the setup tab: the user never holds a proxy account's key.
#[test]
fn a_keyless_proxy_accounts_hint_names_the_re_login() {
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
    let px = crate::profile::load_profile(&name).expect("load profile");
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let readings = snap_with_stats(
        ReadingsSnapshot::default(),
        "px",
        crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );

    assert_eq!(
        status_text(&status_lines(&px, &header, 120, &readings)),
        "status    [ no key ]\n└ no api key set, run `clauth login px --proxy stub`"
    );
}

/// The empty body shares the status hint's words: a keyless proxy account with
/// no figures names its re-login there, with its own name and recorded
/// service, and a keyless endpoint with no record keeps the setup-tab fix in
/// both places, byte for byte.
#[test]
fn the_keyless_fix_reads_alike_in_the_empty_body_and_the_status_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("relay");
    crate::profile::save_profile(&crate::profile::Profile::new(
        "relay".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
    ))
    .expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("zcode").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    let relay = crate::profile::load_profile(&name).expect("load profile");
    let mut generic = crate::testutil::blank_profile(&crate::profile::ProfileName::from("g"));
    generic.base_url = Some("https://example.com".to_string());
    let body = |profile: &crate::profile::Profile| -> Vec<String> {
        build_tp_rows(
            profile,
            120,
            &ReadingsSnapshot::default(),
            false,
            false,
            ResetFmt::default(),
            None,
        )
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .collect()
    };
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let empty_stats = crate::providers::ThirdPartyStats {
        is_available: true,
        rows: vec![],
        bars: vec![],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    let readings = snap_with_stats(
        snap_with_stats(ReadingsSnapshot::default(), "g", empty_stats.clone()),
        "relay",
        empty_stats,
    );

    assert_eq!(
        (body(&relay), body(&generic)),
        (
            vec!["no api key set, run `clauth login relay --proxy zcode`".to_string()],
            vec!["no api key set, add one on the setup tab".to_string()]
        ),
        "(proxy, non-proxy) empty body"
    );
    assert_eq!(
        status_text(&status_lines(&generic, &header, 120, &readings)),
        "status    [ no key ]\n└ no api key set, add one on the setup tab",
        "the non-proxy status hint"
    );
}

/// The `[ no key ]` pill is a third-party state — an OAuth profile never
/// renders it, keyless or not.
#[test]
fn status_lines_oauth_profile_never_renders_no_key() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let rendered = status_text(&status_lines(
        &profile,
        &header,
        120,
        &ReadingsSnapshot::default(),
    ));
    assert!(!rendered.contains("no key"), "got {rendered:?}");
}

/// The disabled rung leads but does NOT erase the health rungs beneath it: a
/// dead login is just as true on a disabled account, and hiding it would strand
/// an operator who re-enables it. Both facts stack on one `├│└` rail.
#[test]
fn status_lines_stacks_the_health_rungs_under_disabled() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("gamma"));
    let header = HeaderState {
        queue_slot: None,
        diag: DiagFlags {
            auth_broken: true,
            ..DiagFlags::default()
        },
        ..disabled_rung_header(false)
    };

    // Control: enabled, the auth-broken rung is the whole block.
    let enabled = status_text(&status_lines(
        &profile,
        &header,
        120,
        &ReadingsSnapshot::default(),
    ));
    assert!(
        !enabled.contains("disabled"),
        "control: an enabled account shows no disabled pill: {enabled:?}"
    );

    profile.disabled = true;
    let lines = status_lines(&profile, &header, 120, &ReadingsSnapshot::default());
    assert_eq!(
        status_text(&lines),
        "status    [ disabled ]\n\
         ├ enable it on the setup tab\n\
         │         [ auth broken ]\n\
         └ re-login with clauth login gamma",
        "both facts stack on one rail"
    );

    // The pill label carries the neutral tier, never danger/warning. Only the
    // fg is worth asserting — every status pill is drawn bold by its caller, so
    // a modifier check would pin that shared choice, not this arm.
    let label = lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "disabled")
        .expect("pill label span renders");
    assert_eq!(
        label.style.fg,
        theme::dim().fg,
        "the disabled pill is neutral (TEXT_DIM), not a fault color"
    );
}

/// The other half of the same ruling: the fetch-state and refresh-countdown
/// rungs ARE suppressed, because polling stops on a disabled account
/// (`usage::scheduler` filters it out of the work list), so "cached" and
/// "refresh in Ns" would both be claims about a poll that will never run.
/// A kick block in the same frame still renders — that one stays true.
#[test]
fn status_lines_suppresses_only_the_fetch_and_refresh_rungs_when_disabled() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let header = disabled_rung_header(true);

    // Control: enabled, both the kick pill and the fetch/countdown rungs render.
    let enabled = status_text(&status_lines(&profile, &header, 120, &readings));
    assert!(
        enabled.contains("cached"),
        "control: an enabled account reports its fetch state: {enabled:?}"
    );
    assert!(
        enabled.contains("blocked"),
        "control: the kick pill renders too: {enabled:?}"
    );

    profile.disabled = true;
    let rendered = status_text(&status_lines(&profile, &header, 120, &readings));
    assert!(
        rendered.contains("blocked"),
        "the kick block stays true and stays visible: {rendered:?}"
    );
    assert!(
        !rendered.contains("cached"),
        "the frozen fetch state is suppressed: {rendered:?}"
    );
    assert!(
        !rendered.contains("refresh in"),
        "the countdown to a poll that never runs is suppressed: {rendered:?}"
    );

    // Disabled with nothing else wrong is a single row and a lone `└`.
    let clean = crate::profile::Profile {
        disabled: true,
        ..crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"))
    };
    assert_eq!(
        status_text(&status_lines(
            &clean,
            &disabled_rung_header(false),
            120,
            &readings
        )),
        "status    [ disabled ]\n└ enable it on the setup tab",
        "no rail when there is nothing to connect"
    );
}

/// A kick-429 block pins its own `[ blocked ]` pill on the row, even
/// while the fetch status reads Fresh — `/usage` stayed 200 through the whole
/// 2026-07-15 messages-limiter outage, so no fetch-status pill can carry this.
/// The suffix names the limiter's advertised ceiling when one was given.
#[test]
fn kick_block_pins_its_own_pill_even_on_a_fresh_row() {
    use crate::usage::KickBlock;

    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Fresh);
    let header = |kick_block: Option<KickBlock>| HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let clean = text(status_lines(&profile, &header(None), 120, &readings));
    assert!(
        !clean.contains("blocked"),
        "no block → no pill, got {clean:?}"
    );

    let now = now_epoch_secs();
    let blocked = text(status_lines(
        &profile,
        &header(Some(KickBlock {
            streak: 2,
            rejected: true,
            until: Some(now + 3 * 60 * 60),
            next_retry: now + 30,
        })),
        120,
        &readings,
    ));
    assert!(
        blocked.contains("[ claude code blocked ]  "),
        "an advertised ceiling trails the pill as a bare suffix, got {blocked:?}"
    );
    assert!(
        !blocked.contains('·'),
        "no middle-dot separator, got {blocked:?}"
    );

    let no_ceiling = text(status_lines(
        &profile,
        &header(Some(KickBlock {
            streak: 1,
            rejected: false,
            until: None,
            next_retry: now + 10,
        })),
        120,
        &readings,
    ));
    assert!(no_ceiling.contains("[ claude code blocked ]"));
    assert!(
        !no_ceiling.contains("[ claude code blocked ]  "),
        "no ceiling → no made-up deadline suffix, got {no_ceiling:?}"
    );
}

/// The block owns the top line and the fetch state drops below it, indented to
/// the value column. Both halves shipped broken behind `contains` assertions:
/// the pill appended straight onto the countdown (`refresh in 14s[ window
/// blocked ]`) because the separator lived inside each suffix's own format
/// string, and at full spread the one-line row ran 83 cells against a detail
/// pane that clears 80 only past a ~123-column terminal, clipping the ceiling
/// off with no wrap. Assert the shape, not just the words.
#[test]
fn the_block_leads_its_own_line_and_never_abuts_the_fetch_state() {
    use crate::usage::KickBlock;

    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::RateLimited);
    let now = now_epoch_secs();
    // 52 inner cells is the narrowest pane the layout builds (an 80-col terminal
    // yields a 24-cell selector), so the block/fetch pills AND their rail hints
    // must all fit — the whole reason this row was split off a single ~83-cell one.
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 14_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts {
                rate_limit: 3,
                refresh_fail: 0,
                last_fetch_failed: false,
            },
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 4 * 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        52,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    // The block pill leads its own keyed line; the fetch pill opens a later line
    // that bridges the rail connecting the two hints between them, so exactly
    // two lines carry a `[ … ]` pill.
    let pill_lines: Vec<&String> = lines.iter().filter(|l| l.contains("[ ")).collect();
    assert_eq!(
        pill_lines.len(),
        2,
        "block pill + fetch pill, got {lines:?}"
    );
    assert!(
        pill_lines[0].starts_with("status") && pill_lines[0].contains("[ claude code blocked ]"),
        "the block leads, keyed: {:?}",
        pill_lines[0]
    );
    assert!(
        pill_lines[1].starts_with(&format!("│{}", " ".repeat(KEY_W + KEY_GUTTER - 1))),
        "the fetch pill bridges the rail between the block's hint and its own, \
         still at the value column: {:?}",
        pill_lines[1]
    );
    assert!(
        pill_lines[1]
            .trim_start_matches('│')
            .trim_start()
            .starts_with("[ rate limited ]"),
        "the fetch pill opens its own line: {:?}",
        pill_lines[1]
    );

    // No segment may touch its neighbour, and every line survives the 52-cell
    // pane — hint lines included (they word-wrap, so the wrap must actually fit).
    for l in &lines {
        assert!(!l.contains("]["), "pills must not abut each other: {l:?}");
        assert!(!l.contains("s["), "a countdown must not abut a pill: {l:?}");
        assert!(
            l.chars().count() <= 52,
            "line clips at 80 cols ({} cells): {l:?}",
            l.chars().count()
        );
    }
}

/// Two or more fix hints in the same status block connect into one rail
/// (`├`/`│`/`└`) instead of floating as separate detached `└` lines: a pill
/// row sitting strictly between the first and last hint bridges the rail at
/// col 0 (`│` + blank padding to the value column), every hint but the last
/// branches off with `├`, and only the last closes the rail with `└`.
#[test]
fn status_lines_connects_two_plus_hints_into_one_rail() {
    use crate::usage::KickBlock;

    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let now = now_epoch_secs();
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 45_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts {
                rate_limit: 0,
                refresh_fail: 3,
                last_fetch_failed: false,
            },
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 3 * 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags {
                auto_start: false,
                budget_spent: true,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    let bridge = format!("│{}", " ".repeat(KEY_W + KEY_GUTTER - 1));

    // Kick + budget-spent + auth-failing: three pills, three hints.
    let pill_lines: Vec<&String> = lines.iter().filter(|l| l.contains("[ ")).collect();
    assert_eq!(
        pill_lines.len(),
        3,
        "kick + budget-spent + auth-failing pills, got {lines:?}"
    );
    assert!(pill_lines[0].starts_with("status"), "{:?}", pill_lines[0]);
    assert!(
        pill_lines[1].starts_with(&bridge) && pill_lines[2].starts_with(&bridge),
        "pill rows sitting between two hints bridge the rail at col 0: {lines:?}"
    );

    let hint_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("├ ") || l.starts_with("└ "))
        .collect();
    assert_eq!(hint_lines.len(), 3, "one hint per pill, got {lines:?}");
    assert!(
        hint_lines[0].starts_with("├ ") && hint_lines[1].starts_with("├ "),
        "every hint but the last branches off the still-open rail: {lines:?}"
    );
    assert!(
        hint_lines[2].starts_with("└ "),
        "only the last hint closes the rail: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with(" └ ")),
        "no detached single-hint lead survives once 2+ hints stack: {lines:?}"
    );
}

/// A lone hint (nothing to connect) stays the plain `└` form anchored at col 0
/// — this block's own key column, not the generic tooltip's one-cell offset
/// for panes whose row opens past col 0.
#[test]
fn status_lines_single_hint_has_no_rail() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Failed);
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 20_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags {
            auth_broken: true,
            ..DiagFlags::default()
        },
        peak: None,
    };
    let lines: Vec<String> = status_lines(&profile, &header, 120, &readings)
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .collect();

    let hint_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("└ ") || l.starts_with("├ ") || l.starts_with("│"))
        .collect();
    assert_eq!(hint_lines.len(), 1, "a single hint, got {lines:?}");
    assert!(
        hint_lines[0].starts_with("└ re-login with clauth login a"),
        "col-0 anchored, no rail glyph needed for a lone hint: {:?}",
        hint_lines[0]
    );
}

/// A wrapped non-last hint carries the rail `│` on its continuation lines, so a
/// multi-line diagnostic reads as one unbroken stroke.
/// Guards `rail_hint_lines`' `cont = "│ "` branch — the width-120 rail test never
/// wraps into it, so a mutation to blank continuations otherwise stays green.
#[test]
fn status_lines_wrapped_non_last_hint_bridges_its_continuation() {
    use crate::usage::KickBlock;
    let now = now_epoch_secs();
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    // Kick (a long hint) + a cached fetch: two hints, so the kick hint is
    // non-last. At 30 cells the kick hint wraps; the fetch's shorter hint does not.
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 30_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: Some(KickBlock {
                streak: 1,
                rejected: true,
                until: Some(now + 3 * 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags {
                auto_start: true,
                ..DiagFlags::default()
            },
            peak: None,
        },
        30,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    assert!(
        lines.iter().any(|l| l.starts_with("├ ")),
        "the non-last kick hint branches off the open rail: {lines:?}"
    );
    // A bridged pill row also opens with `│`, so exclude those by the pill
    // bracket — a pure hint continuation carries none.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("│ ") && !l.contains('[')),
        "its wrapped continuation carries the rail `│` at col 0: {lines:?}"
    );
}

/// A no-hint row sitting AFTER the rail has closed keeps its blank value-column
/// pad, never a stray `│` below the closing `└`.
/// Guards `render_status_rows`' `seen < hint_count` upper bound on the bridge.
#[test]
fn status_lines_no_hint_row_after_closed_rail_stays_unbridged() {
    use crate::usage::KickBlock;
    let now = now_epoch_secs();
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    // Fetch FAILED carries no fix hint and renders last, so with kick +
    // budget-spent hints above it the rail closes on the spend `└` and the
    // failed row sits below the closed rail.
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Failed);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 14_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: Some(KickBlock {
                streak: 1,
                rejected: false,
                until: Some(now + 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags {
                budget_spent: true,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    let failed = lines
        .iter()
        .find(|l| l.contains("[ failed ]"))
        .expect("the failed fetch row renders");
    assert!(
        failed.starts_with(&" ".repeat(KEY_W + KEY_GUTTER)) && !failed.starts_with('│'),
        "a no-hint row below the closed rail keeps blank pad, no stray `│`: {failed:?}"
    );
}

/// A failed account whose leg a left-running fetch worker holds has no deadline
/// published (nothing is coming to fetch it): the fetch row reads `failed` with
/// no retry countdown, and on the instance that fetches it is not `queued`.
#[test]
fn a_failed_fetch_row_with_no_deadline_reads_failed_not_queued() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Failed);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    let failed = lines
        .iter()
        .find(|l| l.contains("[ failed ]"))
        .expect("the failed fetch row renders");
    assert_eq!(
        failed.split_whitespace().collect::<Vec<_>>(),
        ["status", "[", "failed", "]"],
        "the row names the failure and counts nothing down"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.split_whitespace().any(|word| word == "queued")),
        "no row reads queued: {lines:?}"
    );
}

/// The fetch row of an account 4 minutes past its refresh on an instance that
/// runs no countdown, with the account's reading `read_ago_ms` old.
fn overdue_fetch_row(read_ago_ms: u64, stale: bool) -> Vec<String> {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - read_ago_ms),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    if stale {
        readings = snap_with_stale(readings, "a");
    }
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: false,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - 240_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let row = lines
        .iter()
        .find(|l| l.contains("overdue") || l.contains("refresh in"))
        .unwrap_or_else(|| panic!("a fetch row renders: {lines:?}"));
    row.split_whitespace().map(str::to_string).collect()
}

/// An overdue refresh on a stood-down instance says so and dates the reading,
/// never `refresh in 0s`: the reading is old and nothing here will fetch it.
#[test]
fn an_overdue_fetch_row_says_overdue_and_dates_the_reading() {
    let _home = crate::testutil::HomeSandbox::new();
    assert_eq!(
        overdue_fetch_row(330_000, false),
        [
            "status", "◌", "refresh", "overdue", "·", "read", "5m", "ago"
        ],
    );
}

/// The `[ stale ]` pill keeps its own threshold: it prepends the overdue row
/// only once the reading is past it, never for being overdue alone.
#[test]
fn a_stale_overdue_fetch_row_prepends_the_stale_pill() {
    let _home = crate::testutil::HomeSandbox::new();
    assert_eq!(
        overdue_fetch_row(13 * 60_000 + 30_000, true),
        [
            "status", "[", "stale", "]", "◌", "refresh", "overdue", "·", "read", "13m", "ago"
        ],
    );
}

/// The default and `[ stale ]` arms' joined row text, the same fixture the
/// frozen `overdue_fetch_row` renders, returned un-tokenized so the spacing
/// pins byte-exact (the token form collapses whitespace).
fn overdue_joined_row(read_ago_ms: u64, stale: bool) -> String {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - read_ago_ms),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    if stale {
        readings = snap_with_stale(readings, "a");
    }
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: false,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - 240_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    lines
        .iter()
        .find(|l| l.contains("overdue") || l.contains("refresh in"))
        .unwrap_or_else(|| panic!("a fetch row renders: {lines:?}"))
        .clone()
}

/// The default and `[ stale ]` arms' joined spacing is pinned byte-exact: the
/// dot follows the key pad single-spaced, the stale pill prepends with one
/// raw space, and neither may collapse or widen.
#[test]
fn the_overdue_rows_keep_their_join_spacing() {
    let _home = crate::testutil::HomeSandbox::new();
    assert_eq!(
        overdue_joined_row(330_000, false),
        "status    ◌ refresh overdue · read 5m ago",
    );
    assert_eq!(
        overdue_joined_row(13 * 60_000 + 30_000, true),
        "status    [ stale ] ◌ refresh overdue · read 13m ago",
    );
}

/// The fetch row of a stood-down mirror whose published deadline is
/// `deadline_ago_ms` past, with the account's reading `read_ago_ms` old.
fn mirror_fetch_row_at_deadline(deadline_ago_ms: u64, read_ago_ms: u64) -> Vec<String> {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - read_ago_ms),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: false,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - deadline_ago_ms),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let row = lines
        .iter()
        .find(|l| l.contains("overdue") || l.contains("refresh in"))
        .unwrap_or_else(|| panic!("a fetch row renders: {lines:?}"));
    row.split_whitespace().map(str::to_string).collect()
}

/// A mirror's published deadline reads overdue only past the tick's fetch
/// deadline (90 s): 89 s past still draws the countdown form, 91 s the overdue
/// wording. The fixtures sit one second either side of the grace
/// boundary, the margin this pin holds against the wall clock moving between
/// stamp and render.
#[test]
fn a_mirror_fetch_row_reads_overdue_only_past_the_tick_deadline_grace() {
    let _home = crate::testutil::HomeSandbox::new();
    assert_eq!(
        mirror_fetch_row_at_deadline(89_000, 330_000),
        ["status", "◌", "refresh", "in", "0s"],
    );
    assert_eq!(
        mirror_fetch_row_at_deadline(91_000, 330_000),
        [
            "status", "◌", "refresh", "overdue", "·", "read", "5m", "ago"
        ],
    );
}

/// The `[ rate limited ]` suffix names which retry the countdown leads to
/// (`HeaderState.streak`) so a deep slot reads as stuck from the count alone;
/// a zero streak keeps the bare `retry in` suffix.
#[test]
fn rate_limited_suffix_counts_the_retry() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::RateLimited);
    let header = |streak: u32| HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts {
            rate_limit: streak,
            refresh_fail: 0,
            last_fetch_failed: false,
        },
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    assert!(text(status_lines(&profile, &header(7), 120, &readings)).contains("7th retry in"));
    let bare = text(status_lines(&profile, &header(0), 120, &readings));
    assert!(bare.contains("retry in"));
    assert!(
        !bare.contains("th retry"),
        "a zero streak must not invent a retry count"
    );
}

/// A run of transient refresh failures bails to `Cached` — true, we ARE serving
/// last-known numbers — so without this the row says `cached` and nothing names
/// the chain having stopped rotating. `auth failing` claims only what we know:
/// a confirmed-dead token quarantines instead and shows the `×` marker, so this
/// pill must never appear for one.
#[test]
fn a_failing_refresh_names_itself_on_the_cached_row() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let header = |refresh_fail: u32| HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts {
            rate_limit: 0,
            refresh_fail,
            last_fetch_failed: false,
        },
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // No failures: the plain cached row, counting down to a usage refresh.
    let healthy = text(status_lines(&profile, &header(0), 120, &readings));
    assert!(healthy.contains("cached"), "got {healthy:?}");
    assert!(healthy.contains("refresh in"));
    assert!(
        !healthy.contains("auth failing"),
        "a cached row with a healthy chain must not cry auth"
    );

    // Failing: the pill names the cause and the countdown becomes a retry
    // ordinal, since it now leads to the next REFRESH attempt, not a poll.
    let failing = text(status_lines(&profile, &header(3), 120, &readings));
    assert!(failing.contains("auth failing"), "got {failing:?}");
    assert!(failing.contains("3rd retry in"), "got {failing:?}");
    assert!(
        !failing.contains("cached"),
        "the pill states the cause, not the symptom"
    );
}

/// Red is this app's "not recovering on its own" — what `×` (dead login) and
/// `failed` mean. Both streak pills earn it only past the bound the daemon
/// itself stops trusting the reading at (`is_stuck_streak`, the boundary
/// `status.json`'s `stale` keys on); below it they stay amber, or a wifi blip
/// borrows the red that means a dead login and trains the user to ignore it.
#[test]
fn a_streak_pill_turns_red_only_once_it_is_stuck() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let pill_style = |profile: &Profile, header: &HeaderState, readings: &ReadingsSnapshot| {
        status_lines(profile, header, 120, readings)
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("rate limited") || s.content.contains("auth failing"))
            .map(|s| s.style)
            .expect("a streak pill")
    };
    let header = |streaks: StreakCounts| HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };

    for (status, axis) in [
        (
            FetchStatus::RateLimited,
            (|n| StreakCounts {
                rate_limit: n,
                refresh_fail: 0,
                last_fetch_failed: false,
            }) as fn(u32) -> StreakCounts,
        ),
        (FetchStatus::Cached, |n| StreakCounts {
            rate_limit: 0,
            refresh_fail: n,
            last_fetch_failed: false,
        }),
    ] {
        let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
        let readings = snap_with_status(ReadingsSnapshot::default(), "a", status);

        // At the cap the reading is still trusted — amber, same as `cached`.
        assert_eq!(
            pill_style(
                &profile,
                &header(axis(crate::usage::ACTIVE_CAP_MAX_STREAK)),
                &readings
            )
            .fg,
            theme::warning().fg,
            "a streak at the cap is still a staleness cue, not a failure ({status:?})"
        );
        // One past it, the daemon distrusts the number; the row must agree.
        assert_eq!(
            pill_style(
                &profile,
                &header(axis(crate::usage::ACTIVE_CAP_MAX_STREAK + 1)),
                &readings
            )
            .fg,
            theme::danger().fg,
            "a stuck streak must read as red, matching stale/is_stuck_streak ({status:?})"
        );
    }
}

/// `refresh_spent_accounts` OFF drops a spent account's countdown (no pending
/// refresh, `next_refresh_ms` None): the status line renders a bare `[ spent ]`
/// pill instead of the stale "0s" the frozen countdown showed, leaving the reset
/// to the maxed window's own bar line, while a below-cap idle account with no
/// scheduled refresh still reads "up to date".
#[test]
fn spent_skipped_account_pill_is_bare() {
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let with_window = |util: f64| {
        let p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
        let readings = snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            crate::usage::UsageInfo {
                five_hour: Some(crate::usage::UsageWindow {
                    utilization: util,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                ..Default::default()
            },
        );
        (p, readings)
    };

    let (spent_p, spent_readings) = with_window(100.0);
    let spent = text(status_lines(&spent_p, &header, 120, &spent_readings));
    assert!(
        spent.contains("[ spent ]"),
        "a spent skipped account renders the pill: {spent}"
    );
    assert!(
        !spent.contains("resets in"),
        "the reset belongs to the bar line, not the pill: {spent}"
    );
    assert!(
        !spent.contains("0s"),
        "must not freeze at a stale 0s: {spent}"
    );

    let (below_p, below_readings) = with_window(50.0);
    let below = text(status_lines(&below_p, &header, 120, &below_readings));
    assert!(
        below.contains("up to date"),
        "a below-cap idle account with no scheduled refresh is up to date: {below}"
    );
}

/// A spent account the poll skips still carries a `Cached` status when the
/// TUI seeds it off the aged cache (a stood-down TUI, or the boot seed). With
/// no refresh scheduled, no refresh streak and no failed-fetch mark, nothing
/// failed: the row is the same bare `[ spent ]` pill, never `[ cached ]` with
/// "last usage check failed". A spent account WITH a countdown is still
/// polled, so its `Cached` stays the honest serving-old-numbers state.
#[test]
fn a_skipped_spent_account_seeded_cached_reads_spent() {
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let header = |next_refresh_ms: Option<u64>| HeaderState {
        fetches_here: false,
        activity: ProfileActivity::Idle,
        next_refresh_ms,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(
        snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            crate::usage::UsageInfo {
                seven_day: Some(crate::usage::UsageWindow {
                    utilization: 100.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                ..Default::default()
            },
        ),
        "a",
        FetchStatus::Cached,
    );

    let skipped = text(status_lines(&p, &header(None), 120, &readings));
    assert!(
        skipped.contains("[ spent ]"),
        "a skipped spent account reads spent: {skipped}"
    );
    assert!(
        !skipped.contains("[ cached ]"),
        "nothing failed, so no cached pill: {skipped}"
    );
    assert!(
        !skipped.contains("last usage check failed"),
        "a skipped poll is no failed check: {skipped}"
    );

    let polled = text(status_lines(
        &p,
        &header(Some(crate::usage::now_ms() + 60_000)),
        120,
        &readings,
    ));
    assert!(
        polled.contains("[ cached ]"),
        "a spent account still on the schedule keeps its cached state: {polled}"
    );
}

/// A skipped spent account whose last fetch attempt failed keeps the honest
/// `[ cached ]` pill: the failure mark outranks the spent pill exactly as a
/// refresh streak does, so a failed forced `r` stays visible after the next
/// tick drops the countdown. Only the seeded `Cached` (no streak, no mark) is
/// the bare `[ spent ]` pill, the case the test above pins.
#[test]
fn a_skipped_spent_account_whose_last_fetch_failed_reads_cached() {
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts {
            last_fetch_failed: true,
            ..StreakCounts::default()
        },
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(
        snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            crate::usage::UsageInfo {
                seven_day: Some(crate::usage::UsageWindow {
                    utilization: 100.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                ..Default::default()
            },
        ),
        "a",
        FetchStatus::Cached,
    );

    let rendered = text(status_lines(&p, &header, 120, &readings));
    assert!(
        rendered.contains("[ cached ]"),
        "a failed fetch on a skipped spent account reads cached: {rendered}"
    );
    assert!(
        !rendered.contains("[ spent ]"),
        "a failed fetch is never hidden as the spent pill: {rendered}"
    );
}

/// The Usage pane draws the failed-fetch mark from `app.poll_streaks` itself:
/// a skipped spent account whose `Cached` reading follows a failed attempt
/// keeps `[ cached ]`, never the seeded `[ spent ]` pill.
#[test]
fn the_usage_pane_draws_the_failed_fetch_mark_from_the_streak_store() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;
    use crate::usage::{FetchStatus, StreakCounts, UsageInfo, UsageWindow};

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["a"]);
    let name = ProfileName::from("a");
    let mut profile = crate::testutil::blank_profile(&name);
    profile.credentials = Some(crate::testutil::oauth_pair("a"));
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    app.profile_cursor.set(0);
    app.usage_store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            seven_day: Some(UsageWindow {
                utilization: 100.0,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    app.usage_status
        .lock()
        .unwrap()
        .insert("a".to_string(), FetchStatus::Cached);
    app.poll_streaks.lock().unwrap().insert(
        "a".to_string(),
        StreakCounts {
            last_fetch_failed: true,
            ..StreakCounts::default()
        },
    );

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains("[ cached ]")),
        "the mark travels from poll_streaks to the pane's cached pill: {rows:#?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("[ spent ]")),
        "a failed attempt is never hidden as the spent pill: {rows:#?}"
    );
}

/// A skipped spent account that carries a run of failed token refreshes stays
/// on `[ auth failing ]`, never `[ spent ]`: a refresh failure is never hidden
/// as a spent pill. The seeded `Cached` is the only record a refresh is
/// failing, so the guard keeps it live even though the poll skips the account
/// — as the failed-fetch mark does for a failed fetch, the case the test above
/// pins.
#[test]
fn a_skipped_spent_account_with_a_refresh_streak_reads_auth_failing() {
    let text = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let header = HeaderState {
        fetches_here: false,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts {
            refresh_fail: 2,
            ..StreakCounts::default()
        },
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    };
    let p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(
        snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            crate::usage::UsageInfo {
                seven_day: Some(crate::usage::UsageWindow {
                    utilization: 100.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                ..Default::default()
            },
        ),
        "a",
        FetchStatus::Cached,
    );

    let rendered = text(status_lines(&p, &header, 120, &readings));
    assert!(
        rendered.contains("[ auth failing ]"),
        "a refresh-failure streak outranks the spent pill: {rendered}"
    );
    assert!(
        !rendered.contains("[ spent ]"),
        "a refresh-failure streak outranks the spent pill: {rendered}"
    );
}

/// The Usage tab's spent teach names a monthly quota (`monthly quota is
/// spent`), never "weekly limit is spent": the teach producer reads the long
/// window's label, so a monthly-only account at the hard cap gets the monthly
/// arm of the diagnostic.
#[test]
fn the_usage_teach_names_a_monthly_quota() {
    let header = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags {
            weekly_hard: true,
            ..DiagFlags::default()
        },
        peak: None,
    };
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("qwen"));
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "qwen",
        crate::usage::UsageInfo {
            monthly: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    let rendered = status_text(&status_lines(&profile, &header, 120, &readings));
    let hint_line = rendered
        .lines()
        .find(|l| l.contains("is spent"))
        .expect("a spent account renders its hint");
    assert_eq!(hint_line, "└ monthly quota is spent");
}

/// The retry ordinal survives English's teen exceptions (`11th`–`13th` beat
/// the `1`/`2`/`3` last-digit rules).
#[test]
fn ordinal_covers_teens_and_edge_digits() {
    for (n, want) in [
        (1, "1st"),
        (2, "2nd"),
        (3, "3rd"),
        (4, "4th"),
        (11, "11th"),
        (12, "12th"),
        (13, "13th"),
        (21, "21st"),
        (22, "22nd"),
        (23, "23rd"),
        (111, "111th"),
    ] {
        assert_eq!(ordinal(n), want);
    }
}

// ── extra / spend credit bar ──────────────────────────────────────────────────
//
// `extra_usage` (legacy) and `spend` (newer) are the same credit cap on a real
// account. `spend` carries dollars; `extra_usage` reports bare minor units.

/// When both blocks are present the `extra` bar is suppressed (spend owns it),
/// and the legacy fallback scales its cents to dollars instead of showing 100×.
#[test]
fn extra_bar_dedups_against_spend_and_scales_cents() {
    let with = |extra: Option<crate::usage::ExtraUsage>, spend: Option<crate::usage::SpendInfo>| {
        let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
        let readings = snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            crate::usage::UsageInfo {
                plan: None,
                five_hour: None,
                seven_day: None,
                monthly: None,
                weekly_scoped: Vec::new(),
                window_dollars: Vec::new(),
                extra_usage: extra,
                spend,
                codex_limit_reached: None,
                limit_resets: None,
                open_at: None,
                fetched_at: None,
                no_five_hour_limit: false,
            },
        );
        collect_stats(
            readings.usage_of(&profile.name).expect("usage"),
            ResetFmt::default(),
        )
    };
    let extra = crate::usage::ExtraUsage {
        is_enabled: true,
        monthly_limit: Some(5000.0),
        used_credits: Some(487.0),
        utilization: Some(9.74),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let spend = crate::usage::SpendInfo {
        enabled: true,
        used: Some(4.87),
        limit: Some(50.0),
        percent: Some(10.0),
        currency: Some("USD".to_string()),
    };

    // Real account (both blocks): only `spend` renders, no duplicate `extra`.
    let both = with(Some(extra.clone()), Some(spend));
    assert!(both.iter().any(|s| s.label == "spend"));
    assert!(
        !both.iter().any(|s| s.label == "extra"),
        "extra suppressed while spend is visible"
    );

    // Legacy-only account: `extra` falls back, cents scaled to dollars, and the
    // figure rides the trailing line (where window bars show `resets in`).
    let legacy = with(Some(extra), None);
    let bar = legacy.iter().find(|s| s.label == "extra").unwrap();
    assert_eq!(bar.trailing, "$4.87 / $50.00");
    assert!(bar.amount.is_empty());
}

fn credit_stats(
    extra: Option<crate::usage::ExtraUsage>,
    spend: Option<crate::usage::SpendInfo>,
) -> Vec<Stat> {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "a",
        crate::usage::UsageInfo {
            plan: None,
            five_hour: None,
            seven_day: None,
            monthly: None,
            weekly_scoped: Vec::new(),
            window_dollars: Vec::new(),
            extra_usage: extra,
            spend,
            codex_limit_reached: None,
            limit_resets: None,
            open_at: None,
            fetched_at: None,
            no_five_hour_limit: false,
        },
    );
    collect_stats(
        readings.usage_of(&profile.name).expect("usage"),
        ResetFmt::default(),
    )
}

/// The billing-off shape observed live on 2026-07-17: a stale limit and spend
/// left behind on an account whose billing is switched off. That cap binds
/// nothing, so none of its figures may render as if it were live.
fn billing_off_spend() -> crate::usage::SpendInfo {
    crate::usage::SpendInfo {
        enabled: false,
        used: Some(31.07),
        limit: Some(50.0),
        percent: Some(62.0),
        currency: Some("USD".to_string()),
    }
}

#[test]
fn a_billing_off_spend_block_shows_none_of_its_stale_figures() {
    let stats = credit_stats(None, Some(billing_off_spend()));
    for stat in &stats {
        for text in [&stat.trailing, &stat.amount] {
            assert!(
                !text.contains("31.07") && !text.contains("50.00"),
                "a billing-off cap's stale figures must not render: {:?} -> {text:?}",
                stat.label
            );
        }
    }
    assert!(
        !stats
            .iter()
            .any(|s| s.label == "spend" && s.pct.is_some_and(|p| p > 0.0)),
        "a billing-off cap draws no filled bar"
    );
}

#[test]
fn a_billing_off_spend_block_leaves_the_live_legacy_extra_bar_up() {
    let extra = crate::usage::ExtraUsage {
        is_enabled: true,
        monthly_limit: Some(5000.0),
        used_credits: Some(487.0),
        utilization: Some(9.74),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let stats = credit_stats(Some(extra), Some(billing_off_spend()));
    let bar = stats
        .iter()
        .find(|s| s.label == "extra")
        .expect("the live extra bar renders while spend's billing is off");
    assert_eq!(bar.trailing, "$4.87 / $50.00");
}

#[test]
fn an_unread_spend_figure_renders_a_dash_never_zero() {
    let spend = crate::usage::SpendInfo {
        enabled: true,
        used: None,
        limit: Some(50.0),
        percent: Some(10.0),
        currency: Some("USD".to_string()),
    };
    let stats = credit_stats(None, Some(spend));
    let bar = stats
        .iter()
        .find(|s| s.label == "spend")
        .expect("spend renders");
    assert_eq!(bar.trailing, "— / $50.00");
}

#[test]
fn an_unread_legacy_extra_figure_renders_a_dash_never_zero() {
    let extra = crate::usage::ExtraUsage {
        is_enabled: true,
        monthly_limit: None,
        used_credits: Some(487.0),
        utilization: Some(9.74),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let stats = credit_stats(Some(extra), None);
    let bar = stats
        .iter()
        .find(|s| s.label == "extra")
        .expect("extra renders");
    assert_eq!(bar.trailing, "$4.87 / —");
}

/// Render the usage pane (90×24) for one OAuth profile seeded with `u`, as
/// plain per-row strings. The credit rows live in the right-hand detail pane.
fn render_usage_rows(u: crate::usage::UsageInfo) -> Vec<String> {
    render_usage_rows_with_auto_start(u, false)
}

/// [`render_usage_rows`] with the account's `auto_start` opt-in set, so the
/// header's kick countdown is judged both ways.
fn render_usage_rows_with_auto_start(u: crate::usage::UsageInfo, auto_start: bool) -> Vec<String> {
    crate::testutil::buffer_rows(&render_usage_buffer(u, auto_start))
}

/// [`render_usage_rows_with_auto_start`]'s frame kept as a buffer, so a test can
/// read a cell's style as well as its glyph.
fn render_usage_buffer(u: crate::usage::UsageInfo, auto_start: bool) -> ratatui::buffer::Buffer {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["a"]);
    let mut profile = crate::testutil::blank_profile(&ProfileName::from("a"));
    profile.auto_start = auto_start;
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("a")],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    app.profile_cursor.set(0);
    // Both maps hold the reading, as the snapshot builder fills them for an
    // OAuth account: the picked entry IS the raw store entry.
    let readings = snap_with_usage(
        snap_with_oauth_usage(ReadingsSnapshot::default(), "a", u.clone()),
        "a",
        u,
    );
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &readings, &[]))
        .expect("draw");
    term.backend().buffer().clone()
}

/// A plan with no 5h limit keeps its 5h row — the value names the plan in the
/// figure's place, in `TEXT_DIM`, with no bar — and carries no auto-start
/// countdown, since there is no window the kick could open. An unmarked
/// account (a body that merely lost this reading's window) still shows it.
#[test]
fn a_plan_with_no_five_hour_limit_names_the_plan_and_drops_the_auto_start_line() {
    use crate::usage::{UsageInfo, UsageWindow};

    let seven_day = Some(UsageWindow {
        utilization: 0.0,
        resets_at: None,
    });
    let buf = render_usage_buffer(
        UsageInfo {
            seven_day: seven_day.clone(),
            no_five_hour_limit: true,
            ..Default::default()
        },
        true,
    );
    let rows = crate::testutil::buffer_rows(&buf);
    // The bordered pane's 90 cells less its two borders and one padding cell on
    // each side.
    const INNER_W: usize = 86;
    let at = rows
        .iter()
        .position(|row| row.starts_with("│ 5h"))
        .unwrap_or_else(|| panic!("the 5h row stays: {rows:?}"));
    assert_eq!(
        rows[at],
        format!(
            "│ 5h{}no limit on this plan │",
            " ".repeat(INNER_W - "5h".chars().count() - "no limit on this plan".chars().count())
        ),
        "the whole 5h row, value right-aligned in the figure's column: {rows:?}"
    );
    assert!(
        !rows[at].contains('█') && !rows[at].contains('░'),
        "no bar in the value row: {rows:?}"
    );
    assert_eq!(
        rows[at + 1],
        format!("│{}│", " ".repeat(88)),
        "and no bar line under it: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("usage auto-start")),
        "no countdown for a window the plan does not have: {rows:?}"
    );
    // The value's cells carry the secondary text tone: a phrase standing in the
    // figure's place, not the faint dash of a figure clauth failed to read.
    let value: Vec<char> = "no limit on this plan".chars().collect();
    let value_x = rows[at]
        .chars()
        .collect::<Vec<_>>()
        .windows(value.len())
        .position(|w| w == value.as_slice())
        .unwrap_or_else(|| panic!("the value's own cells: {rows:?}"));
    for (dx, ch) in value.iter().enumerate() {
        let cell = buf
            .cell((value_x as u16 + dx as u16, at as u16))
            .expect("a value cell");
        assert_eq!(
            Some(cell.fg),
            theme::dim().fg,
            "the value's cell {ch:?} at {} is TEXT_DIM: {rows:?}",
            value_x + dx
        );
    }

    // Control: the same account unmarked, opted in — the countdown renders.
    let rows = render_usage_rows_with_auto_start(
        UsageInfo {
            seven_day,
            ..Default::default()
        },
        true,
    );
    let expected = format!(
        "│ plan      —{}usage auto-start due now │",
        " ".repeat(
            INNER_W - "plan      —".chars().count() - "usage auto-start due now".chars().count()
        )
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.contains("usage auto-start"))
            .map(String::as_str),
        Some(expected.as_str()),
        "an unmarked windowless account still names its next kick: {rows:?}"
    );
}

/// A plan with no 5h limit takes no burn figure, even when a stored body still
/// carries a 5h window (a header reading's window survives the verdict) and the
/// history series has a slope for it: `· X %/h` beside `no limit on this plan`
/// would name a window the plan does not have.
#[test]
fn a_plan_with_no_five_hour_limit_takes_no_burn_rate() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;
    use crate::usage::{UsageInfo, UsageWindow};

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["a"]);
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("a")],
            ..AppState::default()
        },
        profiles: vec![crate::testutil::blank_profile(&ProfileName::from("a"))],
    });
    app.profile_cursor.set(0);
    let now_ms = crate::usage::now_ms();
    let sample = |utilization: f64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization,
            resets_at: None,
        }),
        ..Default::default()
    };
    // Three distinct samples inside the 1h lookback: enough for a %/h slope.
    app.history_cache.insert(
        "a".to_string(),
        vec![
            (now_ms - 8 * 60_000, sample(1.0)),
            (now_ms - 4 * 60_000, sample(2.0)),
        ],
    );
    let marked = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 3.0,
            resets_at: None,
        }),
        seven_day: Some(UsageWindow {
            utilization: 1.0,
            resets_at: None,
        }),
        no_five_hour_limit: true,
        ..Default::default()
    };
    let readings = snap_with_usage(
        snap_with_oauth_usage(ReadingsSnapshot::default(), "a", marked.clone()),
        "a",
        marked,
    );
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &readings, &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let at = rows
        .iter()
        .position(|row| row.starts_with("│ 5h"))
        .unwrap_or_else(|| panic!("the 5h row stays: {rows:?}"));
    assert_eq!(
        rows[at],
        format!(
            "│ 5h{}no limit on this plan │",
            " ".repeat(86 - "5h".chars().count() - "no limit on this plan".chars().count())
        ),
        "no rate attaches to the plan phrase: {rows:?}"
    );
}

/// The Usage tab's queue countdown for an unmarked opted-in member of a roster
/// that holds a plan with no 5h limit: the render pass builds the roster from
/// the per-frame usage snapshot, so the marked member takes no slot and does not
/// size `N`. A call site handing the roster a constant closure would render the
/// three-member gap here.
#[test]
fn the_usage_tab_sizes_the_queue_without_a_plan_that_has_no_five_hour_limit() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["a", "b", "m"]);
    let queued = |name: &str| {
        let mut p = crate::testutil::blank_profile(&ProfileName::from(name));
        p.auto_start = true;
        p.credentials = Some(crate::testutil::oauth_pair(name));
        p
    };
    let names = || {
        vec![
            ProfileName::from("a"),
            ProfileName::from("b"),
            ProfileName::from("m"),
        ]
    };
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: names(),
            fallback_chain: names(),
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![queued("a"), queued("b"), queued("m")],
    });
    // Row `b`: an unmarked member behind the marked one in queue order.
    app.profile_cursor.set(1);
    let anchor = crate::usage::now_epoch_secs() - 60;
    app.auto_start_queue.lock().unwrap().last_open_at = Some(anchor);
    let rows_of = |readings: &ReadingsSnapshot| -> Vec<String> {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).expect("terminal");
        term.draw(|f| draw_usage_detail(f, f.area(), &app, readings, &[]))
            .expect("draw");
        crate::testutil::buffer_rows(term.backend().buffer())
    };
    // The plan row: `plan` column, no tier known, the countdown flush right in
    // the 86-cell content. Two members that can open a window: 5h/2 less the
    // 90 s interval tolerance (8910 s), minus the 60 s since the anchor.
    let plan_row = |kick: &str| {
        format!(
            "│ plan      —{}{} │",
            " ".repeat(86 - "plan      —".chars().count() - kick.chars().count()),
            kick
        )
    };
    let marked = crate::usage::UsageInfo {
        no_five_hour_limit: true,
        ..Default::default()
    };
    let marked_rows = rows_of(&snap_with_usage(
        snap_with_oauth_usage(ReadingsSnapshot::default(), "m", marked.clone()),
        "m",
        marked,
    ));
    let at = marked_rows
        .iter()
        .position(|row| row.starts_with("│ plan"))
        .unwrap_or_else(|| panic!("the plan row renders: {marked_rows:?}"));
    assert_eq!(
        marked_rows[at],
        plan_row("usage auto-start in 2h 27m"),
        "the marked member sizes no part of the gap: {marked_rows:?}"
    );

    // Control: the same roster with nothing marked is a three-member queue, and
    // the countdown names its shorter gap.
    let unmarked_rows = rows_of(&ReadingsSnapshot::default());
    let at = unmarked_rows
        .iter()
        .position(|row| row.starts_with("│ plan"))
        .unwrap_or_else(|| panic!("the plan row renders: {unmarked_rows:?}"));
    assert_eq!(
        unmarked_rows[at],
        plan_row("usage auto-start in 1h 37m"),
        "unmarked control: three members size the gap: {unmarked_rows:?}"
    );
}

/// A hybrid (an OAuth login plus a provider endpoint) keeps its own plan's
/// verdict in the raw OAuth store while its picked reading is the provider's:
/// the countdown gate and the queue chip read that store, the same one the
/// scheduler's election reads, so the marked account shows no countdown and
/// sizes no part of the next member's gap.
#[test]
fn a_hybrid_plan_with_no_five_hour_limit_is_judged_off_its_oauth_entry() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["a", "b", "m"]);
    let queued = |name: &str| {
        let mut p = crate::testutil::blank_profile(&ProfileName::from(name));
        p.auto_start = true;
        p.credentials = Some(crate::testutil::oauth_pair(name));
        p
    };
    let names = || {
        vec![
            ProfileName::from("a"),
            ProfileName::from("b"),
            ProfileName::from("m"),
        ]
    };
    let mut hybrid = queued("m");
    hybrid.provider = Some(crate::providers::Provider::Zai);
    assert!(
        hybrid.usage_cache_is_third_party(),
        "the picker routes a hybrid to its provider"
    );
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: names(),
            fallback_chain: names(),
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![queued("a"), queued("b"), hybrid],
    });
    let anchor = crate::usage::now_epoch_secs() - 60;
    app.auto_start_queue.lock().unwrap().last_open_at = Some(anchor);
    // What the snapshot builder produces for a hybrid: the picked map holds the
    // provider reading, the OAuth map the raw store entry that carries the plan
    // verdict.
    let readings = snap_with_oauth_usage(
        ReadingsSnapshot::default(),
        "m",
        crate::usage::UsageInfo {
            seven_day: Some(crate::usage::UsageWindow {
                utilization: 1.0,
                resets_at: None,
            }),
            no_five_hour_limit: true,
            ..Default::default()
        },
    );
    let readings = snap_with_usage(
        readings,
        "m",
        crate::usage::UsageInfo {
            seven_day: Some(crate::usage::UsageWindow {
                utilization: 3.0,
                resets_at: None,
            }),
            ..Default::default()
        },
    );
    let mut rows_of = |cursor: usize| -> Vec<String> {
        app.profile_cursor.set(cursor);
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).expect("terminal");
        term.draw(|f| draw_usage_detail(f, f.area(), &app, &readings, &[]))
            .expect("draw");
        crate::testutil::buffer_rows(term.backend().buffer())
    };
    // Row `b`: the marked hybrid takes no slot, so the two live members size the
    // gap — 5h/2 less the 90 s interval tolerance, minus the 60 s since the
    // anchor.
    let rows = rows_of(1);
    let expected = format!(
        "│ plan      —{}usage auto-start in 2h 27m │",
        " ".repeat(
            86 - "plan      —".chars().count() - "usage auto-start in 2h 27m".chars().count()
        )
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.starts_with("│ plan"))
            .map(String::as_str),
        Some(expected.as_str()),
        "the marked hybrid sizes no part of the gap: {rows:?}"
    );
    // Row `m` itself: no countdown for a kick that never fires.
    let rows = rows_of(2);
    assert!(
        !rows.iter().any(|row| row.contains("usage auto-start")),
        "the marked hybrid shows no countdown: {rows:?}"
    );
}

/// A spend block whose billing is off must not dress its stale cap as a live
/// bar: exactly one `spend` note names the state, and no pane row carries a bar
/// glyph or a dollar figure.
#[test]
fn a_billing_off_spend_block_renders_one_note_row_without_bar_or_figures() {
    let rows = render_usage_rows(crate::usage::UsageInfo {
        spend: Some(billing_off_spend()),
        ..Default::default()
    });
    let note_rows: Vec<_> = rows.iter().filter(|r| r.contains("billing off")).collect();
    assert_eq!(note_rows.len(), 1, "exactly one note row: {rows:?}");
    // The whole note row: the label, the note, and nothing else — no bar, no
    // stale figures, no `$`. Padded to the 90-cell pane.
    let expected = format!(
        "│ spend  billing off{}│",
        " ".repeat(90 - "│ spend  billing off│".chars().count())
    );
    assert_eq!(note_rows[0], &expected, "the whole note row: {rows:?}");
    assert!(
        !rows.iter().any(|r| r.contains('█') || r.contains('░')),
        "no bar glyph may render: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains('$')),
        "no dollar figure: {rows:?}"
    );
}

/// An unread `spend.percent` renders `—` faint where the `%` prints and no bar
/// track (figures only) — never `0%`, never a `░`/`█` track.
#[test]
fn an_unread_spend_percent_renders_a_dash_and_no_track() {
    let spend = crate::usage::SpendInfo {
        enabled: true,
        used: Some(31.07),
        limit: Some(50.0),
        percent: None,
        currency: Some("USD".to_string()),
    };
    let lines = render_stat_block(&credit_stats(None, Some(spend)), 36);
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(
        text,
        vec![
            format!("spend{}—", " ".repeat(30)),
            format!("{}{}", " ".repeat(21), "$31.07 / $50.00"),
        ],
        "the whole eyebrow and bar rows, figures only: {text:?}"
    );
}

/// An unread `extra` or per-period utilization renders `—` and no track, the
/// same as an unread spend percent — never `0%`, never a bar graphic.
#[test]
fn an_unread_extra_or_period_utilization_draws_a_dash_and_no_track() {
    let extra = crate::usage::ExtraUsage {
        is_enabled: true,
        used_credits: Some(487.0),
        monthly_limit: Some(5000.0),
        utilization: None,
        daily: Some(serde_json::json!({"used_credits": 200.0})),
        ..Default::default()
    };
    let lines = render_stat_block(&credit_stats(Some(extra), None), 60);
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let eyebrows: Vec<&str> = text
        .iter()
        .filter(|l| l.contains("extra"))
        .map(|l| l.as_str())
        .collect();
    assert_eq!(
        eyebrows.len(),
        2,
        "the extra bar and the period row both render: {text:?}"
    );
    assert_eq!(
        eyebrows,
        vec![
            format!("extra{}—", " ".repeat(54)),
            format!("extra usage (24h){}—", " ".repeat(42)),
        ],
        "each unread utilization renders — where the % prints: {text:?}"
    );
    assert!(
        !text.iter().any(|l| l.contains('░') || l.contains('█')),
        "an unread utilization draws no track: {text:?}"
    );
}

/// A billing-on spend whose money objects are both unread renders `—` for the
/// figures — never an empty trailing, never `$0.00`.
#[test]
fn a_spend_with_both_amounts_unread_renders_a_dash() {
    let spend = crate::usage::SpendInfo {
        enabled: true,
        used: None,
        limit: None,
        percent: Some(40.0),
        currency: Some("USD".to_string()),
    };
    let stats = credit_stats(None, Some(spend));
    let bar = stats.iter().find(|s| s.label == "spend").expect("spend");
    assert_eq!(bar.trailing, "—");
}

/// A per-period row with a read `monthly_limit` and unread `used_credits`
/// renders `— / $limit` — the read limit is kept, the unread used is marked.
#[test]
fn a_period_with_read_limit_and_unread_used_renders_a_dash() {
    let extra = crate::usage::ExtraUsage {
        is_enabled: true,
        used_credits: Some(487.0),
        monthly_limit: Some(5000.0),
        utilization: Some(9.74),
        daily: Some(serde_json::json!({"utilization": 5.0, "monthly_limit": 100.0})),
        ..Default::default()
    };
    let stats = credit_stats(Some(extra), None);
    let period = stats
        .iter()
        .find(|s| s.label == "extra usage (24h)")
        .expect("period renders");
    assert_eq!(period.trailing, "— / $100.00");
}

/// Per-period rows key on `extra.is_enabled` alone, never on spend billing: a
/// disabled extra block hides them, but a billing-on or billing-off spend both
/// keep them — only the legacy `extra` bar dedups against a billing-on spend.
#[test]
fn period_rows_gate_on_extra_enabled_alone() {
    let enabled = || crate::usage::ExtraUsage {
        is_enabled: true,
        daily: Some(serde_json::json!({"utilization": 5.0, "used_credits": 200.0})),
        ..Default::default()
    };
    let spend = || crate::usage::SpendInfo {
        enabled: true,
        used: Some(1.0),
        limit: Some(50.0),
        percent: Some(2.0),
        currency: Some("USD".to_string()),
    };

    // A disabled extra block renders no period rows.
    let disabled = crate::usage::ExtraUsage {
        is_enabled: false,
        daily: Some(serde_json::json!({"utilization": 5.0, "used_credits": 200.0})),
        ..Default::default()
    };
    let stats = credit_stats(Some(disabled), None);
    assert!(
        !stats.iter().any(|s| s.label.starts_with("extra usage")),
        "a disabled extra block renders no period rows"
    );

    // A billing-on spend with an enabled extra hides only the legacy extra bar;
    // the period rows still render.
    let stats = credit_stats(Some(enabled()), Some(spend()));
    let labels: Vec<_> = stats.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["extra usage (24h)", "spend"],
        "a billing-on spend hides the extra bar, not the period rows"
    );

    // A billing-off spend with an enabled extra keeps the extra bar and the
    // period rows.
    let stats = credit_stats(Some(enabled()), Some(billing_off_spend()));
    let labels: Vec<_> = stats.iter().map(|s| s.label.as_str()).collect();
    assert!(
        labels.contains(&"extra"),
        "the extra bar renders: {labels:?}"
    );
    assert!(
        labels.contains(&"extra usage (24h)"),
        "the period row renders beside a billing-off spend: {labels:?}"
    );
}

// ── diagnostic hints ──────────────────────────────────────────────────────────
//
// Each degraded/misconfigured state maps to a `└` fix line naming WHAT is wrong
// and HOW to fix it, varying with config. The flagship is the kick-block split:
// a switch-grade block reads differently under `auto_start` on vs off.

/// The flagship (state, config) → hint divergence: a switch-grade kick block on
/// an `auto_start` account is reassurance (clauth re-tests each poll and it
/// clears itself), on a manual one it names the fix (enable auto-start). The two
/// copies MUST differ — collapsing them to one string is the mutation this guards.
#[test]
fn kick_hint_diverges_on_auto_start() {
    let on = diag_fix(UsageDiag::KickSwitchGrade { auto_start: true }, "a");
    let off = diag_fix(UsageDiag::KickSwitchGrade { auto_start: false }, "a");
    assert_ne!(on, off, "the auto_start split must change the copy");
    assert_eq!(on, "clauth is re-testing periodically");
    assert_eq!(off, "won't recover with auto-start off, enable it");
    // A non-switch-grade burst is neither — low-urgency backoff, no chain switch.
    assert_eq!(
        diag_fix(UsageDiag::KickBurst, "a"),
        "claude code hit a burst limit"
    );
}

/// The auth-broken fix names the exact re-login command for THIS profile, so the
/// account name has to thread through (a generic "re-login" wouldn't).
#[test]
fn auth_broken_hint_names_the_profile() {
    assert_eq!(
        diag_fix(UsageDiag::AuthBroken { proxy: None }, "kerry"),
        format!(
            "re-login with {}",
            crate::tui::render::prose::cmd("clauth login kerry")
        )
    );
}

/// A block on an account the usage probe reads has its own fix line: no kick
/// runs without a pair, so `enable auto-start` would send the operator to a
/// switch that cannot help, and the probe's next good answer lifts the block.
#[test]
fn a_probed_accounts_kick_block_names_the_probe_as_its_way_out() {
    use crate::usage::KickBlock;
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    crate::testutil::make_probed_account(&mut profile);
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Fresh);
    let now = now_epoch_secs();
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::OAuth,
            streaks: StreakCounts::default(),
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 4 * 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let hint = lines
        .iter()
        .find(|l| l.starts_with('└'))
        .unwrap_or_else(|| panic!("a fix line: {lines:?}"));
    assert_eq!(hint, "└ lifts on the usage probe's next good answer");
}

/// The same block once the probe is switched off: nothing re-tests it, since
/// no kick runs without a pair and no probe runs either, so the fix line names
/// the ceiling the block sits out instead of the auto-start switch.
#[test]
fn a_kick_block_with_no_pair_and_the_probe_off_names_its_ceiling() {
    use crate::usage::KickBlock;
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    crate::testutil::make_probed_account(&mut profile);
    profile.usage_probe = false;
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Fresh);
    let now = now_epoch_secs();
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::OAuth,
            streaks: StreakCounts::default(),
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 4 * 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let hint = lines
        .iter()
        .find(|l| l.starts_with('└'))
        .unwrap_or_else(|| panic!("a fix line: {lines:?}"));
    assert_eq!(hint, "└ expires at its ceiling, since no probe reads it");
}

/// The switch alone does not make the probe read an account: with it on, a
/// block on an account with no setup token to send, a disabled one, or one
/// whose token was refused and rests names its ceiling as well. The control, a
/// probed account, names the probe.
#[test]
fn a_kick_block_the_probe_does_not_read_names_its_ceiling_with_the_switch_on() {
    use crate::usage::KickBlock;
    let _home = crate::testutil::HomeSandbox::new();
    let kick_hint = |profile: &crate::profile::Profile, status: FetchStatus| -> String {
        let readings = snap_with_status(ReadingsSnapshot::default(), "a", status);
        let now = now_epoch_secs();
        let lines: Vec<String> = status_lines(
            profile,
            &HeaderState {
                fetches_here: true,
                activity: ProfileActivity::Idle,
                next_refresh_ms: Some(now_ms() + 90_000),
                tick: 0,
                rate_limit_source: super::RateLimitSource::OAuth,
                streaks: StreakCounts::default(),
                kick_block: Some(KickBlock {
                    streak: 2,
                    rejected: true,
                    until: Some(now + 4 * 60 * 60),
                    next_retry: now + 30,
                }),
                queue_slot: None,
                diag: DiagFlags::default(),
                peak: None,
            },
            120,
            &readings,
        )
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .collect();
        // Other pills carry fix lines of their own, so read the block's: the
        // line under its pill, led by `├` when another fix line follows.
        let pill = lines
            .iter()
            .position(|l| l.contains(crate::format::DIAG_KICK))
            .unwrap_or_else(|| panic!("the block's pill: {lines:?}"));
        let hint = lines
            .get(pill + 1)
            .unwrap_or_else(|| panic!("the block's fix line: {lines:?}"));
        hint.strip_prefix("└ ")
            .or_else(|| hint.strip_prefix("├ "))
            .unwrap_or_else(|| panic!("a fix line under the block's pill: {lines:?}"))
            .to_string()
    };
    let mut probed = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    crate::testutil::make_probed_account(&mut probed);
    assert_eq!(
        kick_hint(&probed, FetchStatus::Fresh),
        "lifts on the usage probe's next good answer",
        "the control: an account the probe reads"
    );
    let mut no_token = probed.clone();
    no_token.has_long_lived_sidecar = false;
    let mut disabled = probed.clone();
    disabled.disabled = true;
    for (case, profile, status) in [
        ("no setup token", &no_token, FetchStatus::Fresh),
        ("disabled", &disabled, FetchStatus::Fresh),
        (
            "token refused and resting",
            &probed,
            FetchStatus::AuthExpired,
        ),
    ] {
        assert!(profile.usage_probe, "{case}: the switch stays on");
        assert_eq!(
            kick_hint(profile, status),
            "expires at its ceiling, since no probe reads it",
            "{case}"
        );
    }
}

/// A credit refusal on record names itself on the card, aged, with what keeps
/// the account skipped for credit-billed work: its reading reads clear while
/// the walks pass it over for that work.
/// Pinned row for row at the narrowest pane (52 cells), the countdown row by
/// its layout.
#[test]
fn the_usage_card_names_a_members_credit_refusal() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Fresh);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::OAuth,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                credit_refused_at: Some(now_ms() - 12 * 60 * 1000),
                ..DiagFlags::default()
            },
            peak: None,
        },
        52,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    // The card floors its countdown at render, a moment after the stamp above,
    // so the seconds on the fetch row are the clock's, not this test's.
    let (fetch_row, rows) = lines.split_last().expect("a fetch row");
    assert_eq!(
        rows,
        [
            "status    [ credits refused ]  12m ago",
            "└ skipped for work that may need credits until a",
            "  request on it succeeds, a window reset leaves it",
            "  headroom, or it logs in as another account",
        ]
    );
    assert!(
        fetch_row.starts_with("          ◌ refresh in ") && fetch_row.ends_with('s'),
        "the fetch row keeps its layout: {fetch_row:?}"
    );
}

/// The divergence must reach the rendered row, not just the pure formatter: drive
/// the real `status_lines` dispatch (a kick pill + its `└`) with `auto_start`
/// flipped and read the copy back — a fix that hard-coded one arm reds here too.
#[test]
fn status_lines_renders_the_auto_start_divergence() {
    use crate::usage::KickBlock;
    let now = now_epoch_secs();
    let joined = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<Vec<_>>()
            .join("")
    };
    let render = |auto_start: bool| {
        // A pair stored, the one shape the kick runs on.
        let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
        profile.credentials = Some(crate::testutil::oauth_pair("sk-ant-oat01-TEST"));
        let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Fresh);
        joined(status_lines(
            &profile,
            &HeaderState {
                fetches_here: true,
                activity: ProfileActivity::Idle,
                next_refresh_ms: Some(now_ms() + 90_000),
                tick: 0,
                rate_limit_source: super::RateLimitSource::ThirdParty,
                streaks: StreakCounts::default(),
                kick_block: Some(KickBlock {
                    streak: 2,
                    rejected: true,
                    until: Some(now + 4 * 60 * 60),
                    next_retry: now + 30,
                }),
                queue_slot: None,
                diag: DiagFlags {
                    auto_start,
                    ..DiagFlags::default()
                },
                peak: None,
            },
            120,
            &readings,
        ))
    };
    assert!(render(true).contains("clauth is re-testing periodically"));
    assert!(render(false).contains("won't recover with auto-start off"));
}

/// A DANGER `uncapped` state outranks a spent budget on the same account — an
/// uncapped ceiling can't be "raised to keep serving", so it takes the pill and
/// suppresses the budget-spent one (the two never render together).
#[test]
fn uncapped_outranks_budget_spent_in_the_status_block() {
    let joined = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<Vec<_>>()
            .join("")
    };
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Fresh);
    let out = joined(status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                spend_uncapped: true,
                budget_spent: true,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    ));
    assert!(out.contains("[ uncapped ]") && out.contains("mark an account last resort"));
    assert!(
        !out.contains("[ extra usage spent ]"),
        "uncapped must suppress the extra-usage-spent pill: {out}"
    );
}

/// A refused setup token reads as such on the OAuth row and says how to get a
/// new one, never as a rejected api key the account does not have.
#[test]
fn a_refused_setup_token_names_itself_and_the_mint() {
    let rows = |ls: Vec<Line<'_>>| -> Vec<String> {
        ls.iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect()
    };
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.usage_probe = true;
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::AuthExpired);
    let out = rows(status_lines(
        &profile,
        &HeaderState {
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            fetches_here: true,
            tick: 0,
            rate_limit_source: super::RateLimitSource::OAuth,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    ));
    assert_eq!(
        out,
        vec![
            "status    [ token rejected ]",
            "└ re-mint with clauth login a --setup-token",
        ]
    );
}

/// The status rows the Usage card draws for account `a`, its setup token
/// refused on record, off the TUI's own merge (`App::readings_snapshot`):
/// `pair` stores an OAuth pair beside the token, `status` is what the live
/// store holds, and `next_refresh_ms` the countdown the frame carries.
fn card_rows_of_a_refused_token(
    pair: bool,
    status: Option<FetchStatus>,
    next_refresh_ms: Option<u64>,
) -> Vec<String> {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["a"]);
    let name = ProfileName::from("a");
    let mut profile = crate::testutil::blank_profile(&name);
    if pair {
        profile.credentials = Some(crate::testutil::oauth_pair("a"));
    }
    crate::testutil::refuse_setup_token(&mut profile);
    let app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name],
            ..AppState::default()
        },
        profiles: vec![profile.clone()],
    });
    if let Some(status) = status {
        app.usage_status
            .lock()
            .unwrap()
            .insert("a".to_string(), status);
    }
    let header = HeaderState {
        next_refresh_ms,
        ..idle_header()
    };
    status_lines(&profile, &header, 120, &app.readings_snapshot())
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect()
}

/// A refused setup token beside a stored pair: the pair's `/usage` poll keeps
/// the live store `Fresh` with a refresh lined up, and the card still names
/// the refusal and the mint, as the `⊘` beside the name does.
#[test]
fn the_usage_card_names_a_refused_setup_token_beside_a_fresh_pair() {
    assert_eq!(
        card_rows_of_a_refused_token(true, Some(FetchStatus::Fresh), Some(now_ms() + 90_000)),
        vec![
            "status    [ token rejected ]",
            "└ re-mint with clauth login a --setup-token",
        ]
    );
}

/// A refused setup token with no pair and the probe off: switching the probe
/// off took its status, the record keeps the verdict, and the card says so.
#[test]
fn the_usage_card_names_a_refused_setup_token_with_the_probe_off() {
    assert_eq!(
        card_rows_of_a_refused_token(false, None, None),
        vec![
            "status    [ token rejected ]",
            "└ re-mint with clauth login a --setup-token",
        ]
    );
}

fn idle_header() -> HeaderState {
    HeaderState {
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        fetches_here: true,
        tick: 0,
        rate_limit_source: super::RateLimitSource::OAuth,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
    }
}

fn status_text_of(profile: &crate::profile::Profile, status: FetchStatus) -> String {
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", status);
    status_lines(profile, &idle_header(), 120, &readings)
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect()
}

/// A dead api key reads as one and gets no setup-token mint for a fix.
#[test]
fn a_rejected_key_is_not_told_to_mint_a_setup_token() {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    profile.api_key = Some("key".to_string());
    profile.provider = Some(crate::providers::Provider::Zai);
    let out = status_text_of(&profile, FetchStatus::AuthExpired);
    assert_eq!(out, "status    [ key rejected ]");
}

/// Dead-first: an auth-broken account can't serve regardless of a standing kick
/// block or spend state, so only the `[ auth broken ]` pill + its re-login hint
/// render — the lesser pills are suppressed (mirrors `blocked_reason`'s ranking).
#[test]
fn auth_broken_suppresses_the_lesser_pills() {
    use crate::usage::KickBlock;
    let now = now_epoch_secs();
    let joined = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<Vec<_>>()
            .join("")
    };
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let readings = snap_with_status(ReadingsSnapshot::default(), "a", FetchStatus::Cached);
    let out = joined(status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts {
                rate_limit: 0,
                refresh_fail: 3,
                last_fetch_failed: false,
            },
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 4 * 60 * 60),
                next_retry: now + 30,
            }),
            queue_slot: None,
            diag: DiagFlags {
                auth_broken: true,
                spend_uncapped: true,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    ));
    assert!(
        out.contains("[ auth broken ]") && out.contains("re-login with clauth login a"),
        "the dead login leads: {out}"
    );
    assert!(
        !out.contains("[ claude code blocked ]") && !out.contains("[ uncapped ]"),
        "kick + spend pills are suppressed on a dead login: {out}"
    );
    assert!(
        !out.contains("auth failing"),
        "the confirmed pill supersedes the transient refresh-fail swap: {out}"
    );
    assert!(
        !out.contains("[ cached ]") && !out.contains("refresh in"),
        "the freshness/refresh line is moot on a dead login and stays suppressed: {out}"
    );
}

/// Reproduces the screenshot bug: a dead login with no scheduled refresh and no
/// maxed window used to fall through to the idle `up to date` dot, painting a
/// reassuring state directly under the `[ auth broken ]` pill. Dead-first
/// dominance returns after the pill + hint, so nothing idle leaks below.
#[test]
fn auth_broken_does_not_render_a_reassuring_idle_line() {
    let joined = |ls: Vec<Line<'_>>| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<Vec<_>>()
            .join("")
    };
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("OmniRoute"));
    let out = joined(status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                auth_broken: true,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &ReadingsSnapshot::default(),
    ));
    assert!(
        out.contains("[ auth broken ]") && out.contains("re-login with clauth login OmniRoute"),
        "the dead login still leads with the pill + its re-login hint: {out}"
    );
    assert!(
        !out.contains("up to date"),
        "no idle dot may sit under a dead-login pill: {out}"
    );
}

// ── pricing row (peak-rate indicator) ───────────────────────────────────────

/// `pricing_line` names the peak state as a charged pill plus the countdown
/// to the next flip, all as spans a key/value header row expects. Leaving
/// peak the countdown is relief: a bare `ends in …` reads against the pill,
/// which already names the state.
#[test]
fn pricing_line_peak_pill_and_countdown() {
    let line = pricing_line(crate::pricing::PeakState {
        peak: true,
        next_flip: Some((false, 90 * 60)),
    });
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("pricing"), "key cell present: {text}");
    assert!(text.contains("[ peak rate ]"), "charged peak pill: {text}");
    let countdown = line
        .spans
        .iter()
        .find(|s| s.content.contains("ends in"))
        .expect("a countdown span");
    assert_eq!(countdown.content, "ends in 1h 30m");
    let pill = line
        .spans
        .iter()
        .find(|s| s.content == "peak rate")
        .unwrap();
    assert_eq!(pill.style.fg, theme::warning().fg);
    assert!(
        pill.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

/// Off-peak is the neutral resting state: dim pill, countdown names peak.
#[test]
fn pricing_line_off_peak_pill() {
    let line = pricing_line(crate::pricing::PeakState {
        peak: false,
        next_flip: Some((true, 3600)),
    });
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("[ off-peak ]"),
        "neutral off-peak pill: {text}"
    );
    // Exact-span pin: both arms end in ` in ` (`peak starts in` here, the
    // peak arm's bare `ends in`), so a shared-substring check separates none.
    let countdown = line
        .spans
        .iter()
        .find(|s| s.content.contains("starts in"))
        .expect("a countdown span");
    assert_eq!(countdown.content, "peak starts in 1h 0m");
    let pill = line.spans.iter().find(|s| s.content == "off-peak").unwrap();
    assert_eq!(pill.style.fg, theme::dim().fg);
}

/// No flip inside the query horizon → pill only, no countdown clause.
#[test]
fn pricing_line_without_flip_has_no_countdown() {
    let line = pricing_line(crate::pricing::PeakState {
        peak: true,
        next_flip: None,
    });
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("[ peak rate ]"), "{text}");
    assert!(
        !text.contains("starts in") && !text.contains("ends in"),
        "{text}"
    );
}

/// The row renders between plan and status only when the profile's pricing
/// is time-varying; a `None` peak (flat rates, OAuth, no table) renders no
/// `pricing` key at all.
#[test]
fn header_lines_pricing_row_only_with_windows() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    let base = HeaderState {
        fetches_here: true,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: Some(crate::pricing::PeakState {
            peak: true,
            next_flip: Some((false, 60)),
        }),
    };
    let rows: Vec<String> = header_lines(&profile, &base, 60, &ReadingsSnapshot::default())
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(
        rows.iter().any(|r| r.contains("pricing")),
        "windowed profile gets the row: {rows:?}"
    );
    // The row sits directly under `plan` and above the `status` block.
    let idx = rows.iter().position(|r| r.contains("pricing")).unwrap();
    assert!(idx == 1, "second header row, under plan: {rows:?}");

    let flat = HeaderState { peak: None, ..base };
    let rows: Vec<String> = header_lines(&profile, &flat, 60, &ReadingsSnapshot::default())
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(
        !rows.iter().any(|r| r.contains("pricing")),
        "flat pricing renders no row: {rows:?}"
    );
}

/// A table whose `deepseek` store key holds one model windowed all day — the
/// provider's rows are peak whatever the real clock says.
fn windowed_table_fixture() -> crate::pricing::PriceTable {
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
                    mode: None,
                    window_only: false,
                },
                crate::pricing::PriceEntry {
                    input: 1.0,
                    output: 2.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    constraint: Some(crate::pricing::Constraint::TimeWindow {
                        start: "00:00".to_owned(),
                        end: "24:00".to_owned(),
                    }),
                    mode: None,
                    window_only: false,
                },
            ],
            effective_at: None,
        },
        "2026-01-01",
    )
}

/// `App::peak_state_for` is provider-bound: a profile on a recognized
/// provider's endpoint answers through that provider's store rows, an OAuth
/// profile never does, and pins are irrelevant either way.
#[test]
fn peak_state_for_is_provider_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let table = windowed_table_fixture();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    profile.base_url = Some("https://api.deepseek.com/anthropic".into());
    profile.provider = Some(crate::providers::Provider::DeepSeek);
    let config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile.clone()],
    };
    let mut app = App::new(config);
    assert!(
        app.peak_state_for(&profile).is_none(),
        "no table loaded → no indicator"
    );
    app.price_table = Some(table);
    let s = app
        .peak_state_for(&profile)
        .expect("the provider's store rows answer");
    assert!(s.peak, "the all-day fixture window is always active");

    // Pinning nothing changes nothing: the provider still answers.
    // (The profile above pins nothing already — `blank_profile` — and the
    // provider path answered, which is the unpinned fleet's exact shape.)

    // The provider's own key whose winning row is flat: no indicator.
    let flat_table = crate::pricing::PriceTable::store_key_table(
        "deepseek",
        crate::pricing::PricedModel {
            id: "flat".to_owned(),
            prices: vec![crate::pricing::PriceEntry {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: None,
                mode: None,
                window_only: false,
            }],
            effective_at: None,
        },
        "2026-01-01",
    );
    app.price_table = Some(flat_table);
    assert!(app.peak_state_for(&profile).is_none());

    // No provider at all (OAuth-style): never an indicator — even with the
    // windowed table loaded, pins or no pins.
    app.price_table = Some(windowed_table_fixture());
    let mut bare = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    bare.models.default = Some("deepseek-v4-pro".to_owned());
    assert!(
        app.peak_state_for(&bare).is_none(),
        "a pin is never a provider: no indicator without one"
    );
}

/// Ruling 1 (display parity): the row's stuck streak is OAuth-first with the
/// third-party fallback — the same rule `reading_is_actionable` applies, so a
/// member with no OAuth status entry reads its third-party provider-429 streak.
/// The returned source names which reading won, so the stuck hint names the
/// endpoint that actually throttled (cloudy's "name the winning source").
#[test]
fn effective_rate_limit_is_oauth_first_with_a_third_party_fallback() {
    use crate::profile::ProfileName;
    use std::collections::{HashMap, HashSet};

    let name = ProfileName::from("zai");
    let oauth = HashMap::from([(
        "zai".to_string(),
        StreakCounts {
            rate_limit: 3,
            refresh_fail: 0,
            last_fetch_failed: false,
        },
    )]);
    let third_party = HashMap::from([("zai".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK + 1)]);

    // No OAuth status entry → the third-party streak wins.
    let none = HashSet::new();
    assert_eq!(
        effective_rate_limit(&name, &oauth, &third_party, &none),
        (
            crate::usage::ACTIVE_CAP_MAX_STREAK + 1,
            super::RateLimitSource::ThirdParty
        ),
        "a pure third-party member reads its own provider-429 streak"
    );
    // OAuth status entry present → the OAuth axis is authoritative.
    let has = HashSet::from(["zai".to_string()]);
    assert_eq!(
        effective_rate_limit(&name, &oauth, &third_party, &has),
        (3, super::RateLimitSource::OAuth),
        "a hybrid keeps its OAuth streak authoritative over the third-party one"
    );
}

// ── m3: stuck-429 hint copy (cloudy, verbatim) ──────────────────────────────

/// The complete stuck-429 hint string for one typed provider, exhaustive — a new
/// `Provider` variant breaks this match at compile time, so it cannot ship
/// without its copy fixture.
fn typed_throttler_hint(provider: Provider) -> &'static str {
    match provider {
        Provider::DeepSeek => "deepseek is throttling usage reads",
        Provider::Zai => "z.ai is throttling usage reads",
        Provider::Alibaba => "alibaba model studio is throttling usage reads",
        Provider::OpenRouter => "openrouter is throttling usage reads",
        Provider::MiniMax => "minimax is throttling usage reads",
    }
}

/// A third-party streak names a typed provider's lowercase display name or
/// `the endpoint` for a generic base URL; an OAuth streak reads `anthropic`.
/// Every full hint string is pinned by equality, never by a nonempty/one-example
/// check, so a copy typo reds.
#[test]
fn stuck_429_hint_names_the_throttler() {
    use crate::profile::ProfileName;

    // Exhaustive over the typed providers: the helper's match above fails to
    // compile for a new variant, so this loop can never skip it silently.
    for provider in [
        Provider::DeepSeek,
        Provider::Zai,
        Provider::Alibaba,
        Provider::OpenRouter,
        Provider::MiniMax,
    ] {
        let mut p = crate::testutil::blank_profile(&ProfileName::from("acct"));
        p.provider = Some(provider);
        p.api_key = Some("k".to_string());
        let throttler = throttler_name(&p, super::RateLimitSource::ThirdParty);
        assert_eq!(
            diag_fix(UsageDiag::Stuck429 { throttler }, "acct"),
            typed_throttler_hint(provider),
            "a typed provider's stuck hint names its lowercase display name"
        );
    }

    let mut generic = crate::testutil::blank_profile(&ProfileName::from("gen"));
    generic.api_key = Some("k".to_string());
    generic.base_url = Some("https://example.com/v1".to_string());
    assert_eq!(
        diag_fix(
            UsageDiag::Stuck429 {
                throttler: throttler_name(&generic, super::RateLimitSource::ThirdParty),
            },
            "gen"
        ),
        "the endpoint is throttling usage reads",
        "a generic api-key endpoint reads `the endpoint`"
    );

    let oauth = crate::testutil::blank_profile(&ProfileName::from("oa"));
    assert_eq!(
        diag_fix(
            UsageDiag::Stuck429 {
                throttler: throttler_name(&oauth, super::RateLimitSource::OAuth),
            },
            "oa"
        ),
        "anthropic is throttling usage reads",
        "an OAuth streak reads `anthropic`"
    );
}

/// N-hybrid: a hybrid row (OAuth pair + api key + typed provider) must name the
/// reading source that won, never the row's provider. Both directions are
/// pinned: OAuth stuck / provider healthy reads `anthropic`; OAuth absent /
/// provider stuck reads the typed provider — on the SAME hybrid profile shape.
#[test]
fn a_hybrid_stuck_hint_names_the_winning_reading_source() {
    use crate::profile::ProfileName;
    use std::collections::{HashMap, HashSet};

    let name = ProfileName::from("hybrid");
    // A hybrid row: typed provider (z.ai) + an api key. The OAuth status store
    // decides the source; the provider is present either way, so a row-type
    // throttler would misname the OAuth leg.
    let mut hybrid = crate::testutil::blank_profile(&name);
    hybrid.provider = Some(Provider::Zai);
    hybrid.api_key = Some("k".to_string());
    hybrid.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    let deep = crate::usage::ACTIVE_CAP_MAX_STREAK + 1;

    // OAuth leg deep-stuck, provider leg healthy → the OAuth streak wins.
    let oauth = HashMap::from([(
        "hybrid".to_string(),
        StreakCounts {
            rate_limit: deep,
            refresh_fail: 0,
            last_fetch_failed: false,
        },
    )]);
    let healthy_tp = HashMap::from([("hybrid".to_string(), 1u32)]);
    let oauth_has = HashSet::from(["hybrid".to_string()]);
    let (streak, source) = effective_rate_limit(&name, &oauth, &healthy_tp, &oauth_has);
    assert_eq!(
        (streak, source),
        (deep, super::RateLimitSource::OAuth),
        "an OAuth status entry is authoritative even over a healthy provider leg"
    );
    assert_eq!(
        diag_fix(
            UsageDiag::Stuck429 {
                throttler: throttler_name(&hybrid, source),
            },
            "hybrid"
        ),
        "anthropic is throttling usage reads",
        "the OAuth-stuck hybrid names anthropic, not its provider"
    );

    // OAuth leg absent, provider leg deep-stuck → the third-party streak wins.
    let none = HashSet::new();
    let stuck_tp = HashMap::from([("hybrid".to_string(), deep)]);
    let (streak, source) = effective_rate_limit(&name, &oauth, &stuck_tp, &none);
    assert_eq!(
        (streak, source),
        (deep, super::RateLimitSource::ThirdParty),
        "with no OAuth status entry the third-party streak wins"
    );
    assert_eq!(
        diag_fix(
            UsageDiag::Stuck429 {
                throttler: throttler_name(&hybrid, source),
            },
            "hybrid"
        ),
        "z.ai is throttling usage reads",
        "the third-party-stuck hybrid names its provider"
    );
}

// ── m4: TestBackend row render pin ──────────────────────────────────────────

/// A stuck third-party row renders the red `rate limited` pill, the 7th-retry
/// ordinal, and the provider hint — one frame, so the wiring from the
/// third-party streak store to the row cannot drift green (T-W / T-K).
#[test]
fn a_stuck_third_party_row_renders_the_pill_ordinal_and_hint() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::{FetchLeg, FetchStatus, now_ms};

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["zai"]);

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    app.profile_cursor.set(0);
    app.third_party_streaks
        .lock()
        .unwrap()
        .insert("zai".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK + 1);
    // The third-party status store carries the member's `RateLimited` entry; the
    // T-K plant (reading `oauth_status_has` from it) must flip the row off the
    // third-party streak, so seeding it here makes that plant red.
    app.third_party_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::RateLimited);
    app.next_refresh_per_profile.lock().unwrap().insert(
        FetchLeg::ThirdParty.key(ProfileName::from("zai")),
        now_ms() + 40_000,
    );

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());

    let fetch_row = rows
        .iter()
        .find(|r| r.contains("rate limited"))
        .expect("the fetch row renders the rate limited pill");
    assert!(
        fetch_row.contains("7th retry in"),
        "a deep streak reads as a 7th retry ordinal: {fetch_row:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("z.ai is throttling usage reads")),
        "the hint names z.ai as the throttler: {rows:?}"
    );
}

/// F4: the key-rejection fix copy survives a 45-column pane. The 46-cell
/// no-cache line used to render as one unwrapped `Line` and clip its tail at
/// the 41 interior cells; it now wraps through the shared `panes::wrap_words`
/// path (greedy wrap at 41: `… on the` / `setup tab`), so the full repair
/// instruction stays on screen.
#[test]
fn the_key_rejection_fix_copy_survives_a_narrow_usage_pane() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["zai"]);

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    app.third_party_status
        .lock()
        .unwrap()
        .insert("zai".into(), FetchStatus::AuthExpired);
    app.profile_cursor.set(0);

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());

    assert!(
        rows.iter().any(|r| r.starts_with("│ setup tab")),
        "the clipped tail `setup tab` must wrap onto its own line: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("api key rejected, re-enter it")),
        "the wrapped head keeps the diagnosis: {rows:?}"
    );
}

/// R6: the OAuth no-login fix copy survives a 45-column pane. The 43-cell
/// `not logged in, use + login on the setup tab` message used to render as
/// one unwrapped `Line` behind a 2-cell indent (45 cells in the 41-cell
/// interior: 45 cols − 2 border − 2 block padding) and clip its ` tab` tail;
/// it now wraps through the shared `panes::wrap_words` path like the
/// third-party arm, at the interior minus the 2-cell indent (greedy wrap at
/// 39: `… the setup` / `tab`), the indent surviving on both segments.
#[test]
fn the_oauth_no_login_fix_copy_survives_a_narrow_usage_pane() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["oau"]);

    // A credential-less profile: no login, no usage, no endpoint — the OAuth
    // arm's terminal `oauth_empty_msg` body.
    let oau = crate::testutil::blank_profile(&ProfileName::from("oau"));

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("oau")],
            ..AppState::default()
        },
        profiles: vec![oau],
    });
    app.profile_cursor.set(0);

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());

    // Border `│` + 1 block-padding cell + the 2-cell indent = 3 leading cells.
    assert!(
        rows.iter()
            .any(|r| r.starts_with("│   not logged in, use + login on the setup")),
        "the wrapped head keeps the diagnosis and its 2-cell indent: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.starts_with("│   tab")),
        "the clipped ` tab` tail must wrap onto its own indented line: {rows:?}"
    );
}

/// R7: the best-effort report footer survives a 45-column pane. The 56-cell
/// `looks wrong? report at github.com/uwuclxdy/clauth/issues` line used to
/// render as one unwrapped `Line` and clip mid-`uwuclxdy` at the 41-cell
/// interior (45 cols − 2 border − 2 block padding), dropping the tail of the
/// only in-app report pointer; it now wraps through the shared
/// `panes::wrap_words` path like the terminal arms (greedy wrap at 41:
/// `looks wrong? report at` / `github.com/uwuclxdy/clauth/issues`), the URL
/// surviving whole on its own segment.
#[test]
fn the_best_effort_report_footer_survives_a_narrow_usage_pane() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::{ThirdPartyStats, UsageBar};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["unk"]);

    // An unrecognised endpoint: `provider` stays None, the generic api-key
    // pair routes the body through the third-party arm
    // (`usage_cache_is_third_party`), and `best_effort` bars are exactly the
    // shape the report footer qualifies.
    let mut unk = crate::testutil::blank_profile(&ProfileName::from("unk"));
    unk.base_url = Some("https://generic.example.com/anthropic".to_string());
    unk.api_key = Some("k".to_string());

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("unk")],
            ..AppState::default()
        },
        profiles: vec![unk],
    });
    app.third_party_usage_store.lock().unwrap().insert(
        "unk".into(),
        ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![UsageBar {
                label: "quota".to_string(),
                pct: 42.0,
                resets_at: None,
                used: None,
                total: None,
                inference_quota: false,
            }],
            plan: None,
            endpoint: None,
            best_effort: true,
        },
    );
    app.profile_cursor.set(0);

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());

    assert!(
        rows.iter()
            .any(|r| r.starts_with("│ github.com/uwuclxdy/clauth/issues")),
        "the 33-cell report URL must wrap whole onto its own line, never clip mid-url: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.starts_with("│ looks wrong? report at")),
        "the wrapped head keeps the report invite: {rows:?}"
    );
}

#[test]
fn usage_detail_scroll_reaches_the_final_note_without_changing_account() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::{Tab, handle_key};
    use crate::usage::{ScopedWindow, UsageInfo, UsageWindow};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["many"]);
    let name = ProfileName::from("many");
    let profile = crate::testutil::blank_profile(&name);
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    app.usage_store.lock().unwrap().insert(
        "many".into(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 23.0,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: 37.0,
                resets_at: None,
            }),
            weekly_scoped: (0..12)
                .map(|n| ScopedWindow {
                    label: format!("7d scope-{n:02}"),
                    window: UsageWindow {
                        utilization: 42.0,
                        resets_at: None,
                    },
                })
                .collect(),
            ..UsageInfo::default()
        },
    );
    app.tab = Tab::Usage;
    app.note_text = Some("last visible note".to_string());
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let first = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        !first.concat().contains("last visible note"),
        "the long pane begins at its first window"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    for _ in 0..70 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let last = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        last.concat().contains("last visible note"),
        "scroll reaches the final note"
    );
    assert_eq!(
        app.profile_cursor.read(),
        0,
        "detail scrolling keeps the account selected"
    );
    let (selector, detail) = super::master_detail(ratatui::layout::Rect::new(0, 0, 80, 14), 1);
    let bar_x = detail.x + detail.width - 2;
    assert_eq!(
        term.backend().buffer().cell((bar_x, 12)).unwrap().symbol(),
        "┃",
        "the bottom scrollbar thumb reaches the last content line"
    );
    assert_eq!(
        term.backend()
            .buffer()
            .cell((selector.x + 1, 1))
            .unwrap()
            .symbol(),
        " ",
        "selector has no focus caret while detail scrolls"
    );
    assert_eq!(
        term.backend().buffer().cell((detail.x, 0)).unwrap().fg,
        theme::line_strong_color(),
        "the focused detail border is strong"
    );
    assert_eq!(
        term.backend().buffer().cell((selector.x, 0)).unwrap().fg,
        theme::line_color(),
        "the selector border is blurred"
    );
    term.backend_mut().resize(80, 40);
    term.autoresize().unwrap();
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    assert_eq!(
        app.usage_detail_max_scroll.get(),
        11,
        "resized viewport publishes the smaller bound"
    );
    let at = app.usage_detail_scroll;
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert!(
        app.usage_detail_scroll < at,
        "↑ moves from the resized bound"
    );
}

#[test]
fn usage_detail_reaches_note_after_more_than_u16_rows() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::{StatusFocus, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("long-note");
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name.clone()],
            ..AppState::default()
        },
        profiles: vec![crate::testutil::blank_profile(&name)],
    });
    app.tab = Tab::Usage;
    app.note_text = Some(format!("{}final note row", "x\n".repeat(66_000)));
    assert_eq!(notes_lines(app.note_text.as_deref(), 50).len(), 66_001);
    app.usage_detail_focus = StatusFocus::Detail;
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    app.usage_detail_scroll = usize::from(u16::MAX);
    for _ in 0..80 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    }
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    assert!(
        crate::testutil::buffer_rows(term.backend().buffer())
            .concat()
            .contains("final note row"),
        "even an unbounded note's tail is reachable"
    );
}

#[test]
fn fitting_usage_detail_has_no_scrollbar() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("short");
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name.clone()],
            ..AppState::default()
        },
        profiles: vec![crate::testutil::blank_profile(&name)],
    });
    app.tab = crate::tui::app::Tab::Usage;
    let mut term = Terminal::new(TestBackend::new(80, 40)).unwrap();
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let (_, detail) = super::master_detail(ratatui::layout::Rect::new(0, 0, 80, 40), 1);
    assert_eq!(
        app.usage_detail_max_scroll.get(),
        0,
        "fitting account has no scroll bound"
    );
    assert_eq!(
        term.backend()
            .buffer()
            .cell((detail.x + detail.width - 2, 10))
            .unwrap()
            .symbol(),
        " ",
        "no scrollbar track when content fits"
    );
}

/// The `notes` row: label key + faint hint while the account has none; the
/// full note wrapped to the value column, continuation lines aligned under it.
#[test]
fn the_notes_row_hints_when_empty_and_renders_the_full_note() {
    // No note: the hint line, key in label, hint in faint.
    let lines = notes_lines(None, 60);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
        .collect();
    assert_eq!(lines[0].spans[0].content, "notes     ");
    assert!(all.contains("press n to add notes"));
    assert_eq!(lines[0].spans[0].style, theme::label());
    assert_eq!(lines[0].spans[1].style.fg, Some(theme::text_faint_color()));

    // A note renders IN FULL: every line, wrapped to the value column, with
    // continuation lines padded to the value column.
    let note = "first line\nsecond line that is quite long and keeps going well past the column";
    let lines = notes_lines(Some(note), 40);
    let texts: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .collect();
    assert_eq!(lines[0].spans[0].content, "notes     ");
    assert_eq!(lines[0].spans[1].content, "first line");
    assert_eq!(lines[1].spans[0].content, " ".repeat(NOTES_VALUE_LEAD));
    assert_eq!(lines[1].spans[1].content, "second line that is quite long");
    // value column width = 40 - 10 = 30; the 74-char line wraps greedily to 3.
    assert_eq!(lines.len(), 4, "full note, wrapped: {texts:?}");
    assert_eq!(lines[0].spans[1].style.fg, Some(theme::text_color()));
    assert_eq!(lines[1].spans[1].style.fg, Some(theme::text_color()));
}

/// The note slot renders the `✎` mark, the draft rows and the native caret on
/// the draft row the caret sits in.
#[test]
fn the_note_slot_renders_the_draft_and_parks_the_caret_on_it() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let buf = NoteBuffer::new("ab\ncd");
    let mut term = Terminal::new(TestBackend::new(30, 5)).unwrap();
    term.draw(|f| draw_note_slot(f, f.area(), &buf, &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let screen = rows.concat();
    assert!(screen.contains("✎"), "the edit mark rides the title break");
    assert!(screen.contains("ab"), "draft row one, got {screen:?}");
    assert!(screen.contains("cd"), "draft row two");
    let caret = term.get_cursor_position().unwrap();
    assert_eq!(caret.x, 4, "border + padding + caret display col 2");
    assert_eq!(caret.y, 2, "border + draft row one");
}

/// S10: a draft line wider than the slot scrolls sideways with the caret: End
/// sits inside the slot with the line's two-cell rocket whole before it, and
/// one ← lands on the rocket without moving a drawn cell of the row.
#[test]
fn a_note_line_wider_than_the_slot_scrolls_with_its_caret() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut buf = NoteBuffer::new(&format!("{}🚀", "m".repeat(60)));
    let mut term = Terminal::new(TestBackend::new(30, 5)).unwrap();

    let (end, screen) = draw_caret(&mut term, |f| draw_note_slot(f, f.area(), &buf, &[]));
    let border = right_border(&screen, end.y);
    assert!(
        end.x + 2 <= border,
        "End stays inside the slot ({end:?}, border {border})"
    );
    assert_eq!(
        symbol_at(&screen, end.x - 2, end.y),
        "🚀",
        "the rocket draws whole before End"
    );
    let end_row = crate::testutil::buffer_rows(&screen)[end.y as usize].clone();

    buf.left();
    let (left, screen) = draw_caret(&mut term, |f| draw_note_slot(f, f.area(), &buf, &[]));
    assert_eq!(
        left,
        ratatui::layout::Position::new(end.x - 2, end.y),
        "one left steps over the rocket's two cells"
    );
    assert_eq!(
        symbol_at(&screen, left.x, left.y),
        "🚀",
        "the caret sits on it"
    );
    assert_eq!(
        crate::testutil::buffer_rows(&screen)[left.y as usize],
        end_row,
        "a move inside the window leaves the row alone"
    );
}

/// Past 8 draft rows (or a short slot) the draft scrolls cursor-first.
#[test]
fn the_note_slot_scrolls_the_draft_to_keep_the_caret_visible() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let draft = (0..12)
        .map(|i| format!("row{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let buf = NoteBuffer::new(&draft);
    let mut term = Terminal::new(TestBackend::new(30, 6)).unwrap();
    term.draw(|f| draw_note_slot(f, f.area(), &buf, &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let screen = rows.concat();
    assert!(
        screen.contains("row11"),
        "the caret row stays visible, got {screen:?}"
    );
    let caret = term.get_cursor_position().unwrap();
    assert_eq!(caret.y, 4, "the caret parks on the inner bottom row");
}

/// Keys and commands inside prose on the Usage tab: the auth-broken
/// fix's `clauth login <name>`, the console-login messages' `clauth login` and
/// the empty notes row's `n`, each in its prose's own line.
#[test]
fn usage_prose_styles_its_commands_and_the_notes_key() {
    use crate::testutil::{assert_prose_part, lines_buffer};
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let rail = rail_hint_lines(
        &diag_fix(UsageDiag::AuthBroken { proxy: None }, "kerry"),
        60,
        false,
    );
    assert_prose_part(
        &lines_buffer(rail, 60),
        "re-login with clauth login kerry",
        "clauth login kerry",
        false,
        crate::tui::theme::faint(),
    );

    let mut qwen = crate::testutil::blank_profile(&crate::profile::ProfileName::from("qwen"));
    qwen.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    qwen.provider = crate::providers::Provider::from_base_url(
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
    );
    qwen.console = Some(crate::profile::ConsoleCredential {
        token: "dead".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    let readings = snap_with_status(
        ReadingsSnapshot::default(),
        "qwen",
        FetchStatus::AuthExpired,
    );
    let rows = build_tp_rows(
        &qwen,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    );
    assert_prose_part(
        &lines_buffer(rows, 52),
        "console login expired, run clauth login",
        "clauth login",
        false,
        crate::tui::theme::faint(),
    );
    qwen.console = None;
    let rows = build_tp_rows(
        &qwen,
        52,
        &readings,
        false,
        false,
        ResetFmt::default(),
        None,
    );
    assert_prose_part(
        &lines_buffer(rows, 52),
        "console login needed, run clauth login",
        "clauth login",
        false,
        crate::tui::theme::faint(),
    );

    assert_prose_part(
        &lines_buffer(notes_lines(None, 60), 60),
        "press n to add notes",
        " n ",
        true,
        crate::tui::theme::faint(),
    );
}

/// The eyebrow `%` figure drops to its own line, whole, carrying the amount
/// with it, once the label + rate + amount no longer leave it room — never a
/// clipped `24.5`, and never a dropped amount.
#[test]
fn the_eyebrow_percent_drops_to_its_own_line_when_narrow() {
    let stat = Stat {
        label: "7d".to_string(),
        pct: Some(24.5),
        amount: "12.2".to_string(),
        color: theme::body(),
        burn_rate: Some(12.2),
        rate_unit: "d",
        pace_pct: None,
        reset_secs: None,
        trailing: String::new(),
        note: None,
        value: None,
    };
    let max_rate_w = stat.rate_section_width();
    let lines = stat.render(8, 20, max_rate_w, 5);
    let texts: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(
        texts,
        [
            "7d · 12.2 %/d · 6d 4h left",
            "         12.2  24.5%",
            "██░░░░░░"
        ]
    );
}

/// The `notes` add-hint wraps on a narrow pane instead of clipping its tail.
#[test]
fn the_notes_hint_wraps_on_a_narrow_pane() {
    let lines = notes_lines(None, 24);
    let texts: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(texts, ["notes     press n to add", "          notes"]);
}

/// The kick pill's right pad is `avail − cells`, not `avail − chars`, so a wide
/// (non-ASCII) text still lands flush against the pane edge. Today's `kick_text`
/// is ASCII, so this is the one surface where the cell switch is invisible.
#[test]
fn kick_spans_pads_by_cells() {
    let spans = super::kick_spans("日本語の上流", 0, 30);
    let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(ratatui::text::Span::raw(&joined).width(), 30);
}

// ── tps: the `speed` and `rate` rows ─────────────────────────────────────

fn tps_line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// A fresh snapshot for `uwuclxdy` with live/m5/h1 figures and no day sums.
fn tps_full_snapshot(age_ms: u64) -> crate::tps::Snapshot {
    let stamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(crate::usage::now_ms() - age_ms).unwrap(),
    )
    .unwrap()
    .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
    .to_string();
    let fig = |rate: f64, speed: Option<f64>| crate::tps::Figure { rate, speed };
    let mut snap = crate::tps::Snapshot {
        version: crate::tps::CACHE_VERSION,
        generated_at: stamp,
        ..crate::tps::Snapshot::default()
    };
    snap.accounts.insert(
        "uwuclxdy".into(),
        crate::tps::Figures {
            live: fig(67.2, Some(92.4)),
            m5: fig(61.0, Some(88.0)),
            h1: fig(58.0, Some(84.0)),
        },
    );
    snap
}

/// The `speed` and `rate` rows carry all four windows, `—` faint where absent,
/// and shed whole windows from the right when the pane cannot hold them.
#[test]
fn the_tps_rows_render_four_windows_and_a_dash_where_absent() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();
    let profile = Profile::new("uwuclxdy".to_string(), None, None);
    let config = AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile.clone()],
    };
    let mut app = App::new(config);
    app.set_tps(Some(tps_full_snapshot(1_000)));

    // No day sums: the speed row's `today` reads `—`, but a fresh account's
    // today RATE is a real zero, never an absence.
    let rows = tps_rows(&profile, &app, 100);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live 92 t/s · 5m 88 t/s · 1h 84 t/s · today — · peak —"
    );
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live 67 tok/s · 5m 61 tok/s · 1h 58 tok/s · today 0 tok/s"
    );

    // A day sum of 900 tokens over 10 s: the speed row's `today` is the
    // deterministic 900 ÷ 10 = 90 t/s.
    let mut snap = tps_full_snapshot(1_000);
    snap.days.insert(
        crate::tokens::today_date(),
        std::collections::BTreeMap::from([(
            "uwuclxdy".to_string(),
            std::collections::BTreeMap::from([(
                "sonnet".to_string(),
                crate::tps::DaySum {
                    output: 900,
                    gen_ms: 10_000,
                    ..Default::default()
                },
            )]),
        )]),
    );
    // Pin the clock to 900 s after 00:00 UTC today, and stamp the snapshot 1 s
    // before it, so it reads fresh under the pinned clock and the today rate
    // divides by exactly 900.
    let now = crate::usage::now_ms();
    let fixed = now - (now % 86_400_000) + 900_000;
    snap.generated_at =
        chrono::DateTime::from_timestamp_millis(i64::try_from(fixed - 1_000).unwrap())
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
            .to_string();
    app.set_tps(Some(snap));
    app.tps_now_override = Some(fixed);
    let rows = tps_rows(&profile, &app, 100);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live 92 t/s · 5m 88 t/s · 1h 84 t/s · today 90 t/s · peak —"
    );
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live 67 tok/s · 5m 61 tok/s · 1h 58 tok/s · today 1.0 tok/s"
    );

    // A pane too narrow sheds whole windows from the right, never a partial
    // figure; both rows shed to the same (rate row's) window set.
    let rows = tps_rows(&profile, &app, 50);
    assert_eq!(tps_line_text(&rows[0]), "speed     live 92 t/s · 5m 88 t/s");
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live 67 tok/s · 5m 61 tok/s"
    );
}

/// Under a fresh producer, an account ABSENT from the cache (`accounts` names
/// only accounts with finished responses) still has a real zero rate on every
/// window — rate is never absent under a fresh producer, `which --json` already
/// says `rate: 0` — while its speed windows stay `—` (no response touched
/// them). With no fresh producer behind the cache every figure reads `—`.
#[test]
fn tps_rows_reads_zero_rate_for_a_fresh_producers_absent_account() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();
    let profile = Profile::new("absent".to_string(), None, None);
    let config = AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile.clone()],
    };
    let mut app = App::new(config);

    // The cache names only `uwuclxdy`; `absent` is missing from it.
    app.set_tps(Some(tps_full_snapshot(1_000)));
    let rows = tps_rows(&profile, &app, 100);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live — · 5m — · 1h — · today — · peak —",
        "speed stays absent while no response touches its windows"
    );
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live 0 tok/s · 5m 0 tok/s · 1h 0 tok/s · today 0 tok/s",
        "a fresh producer's absent account still reads a zero rate"
    );

    // No fresh producer: every figure reads `—`.
    app.set_tps(None);
    let rows = tps_rows(&profile, &app, 100);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live — · 5m — · 1h — · today — · peak —"
    );
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live — · 5m — · 1h — · today —"
    );
}

/// The note editor's caret that lands inside a drawn toast hides the native
/// cursor this frame, and it hides under an open modal too. Three stacked
/// DANGER toasts cover the note slot's caret column at 45x14.
#[test]
fn a_toast_over_the_note_editor_caret_hides_the_native_cursor() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::{Modal, NoteEditor, Tab, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    let name = ProfileName::from("a");
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name.clone()],
            ..AppState::default()
        },
        profiles: vec![crate::testutil::blank_profile(&name)],
    });
    app.tab = Tab::Usage;
    app.note_editor = Some(NoteEditor {
        profile: name.clone(),
        seed: "ab".to_string(),
        buf: NoteBuffer::new("ab"),
    });
    app.usage_detail_focus = StatusFocus::Detail;

    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the note caret shows with no toasts"
    );

    let wide = "x".repeat(53);
    for label in ["one", "two", "three"] {
        app.toast(ToastKind::Danger, format!("{label}\n{wide}\n{label} tail"));
    }

    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "[45x14] the note caret hides under a toast"
    );

    app.toasts.clear();
    app.open_modal(Modal::Help);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "the note caret hides under an open modal"
    );
}

// ── codex and pool accounts on the Usage tab (design record "Settled: usage") ──

/// A pool account with every flag clear and no readings.
fn t6_pool_account(name: &str) -> crate::gateway_admin::PoolAccount {
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
        requests_attempted: None,
        requests_failed: None,
        requests_cancelled: None,
        mean_latency_ms: None,
    }
}

/// One claude account `cl1`, one codex account `cx1` (plus, 5h 42%, 7d 12%,
/// two banked resets, polled 2 h ago) and one pool provider `claude-main`
/// serving `alpha` (max, 5h 73%, 7d 41%, 7d fable 18%) and `beta` (cooling
/// down 240 s), the pool listing read 2 min ago; the Usage tab shown.
fn t6_app() -> App {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    use crate::profile::{AppConfig, AppState, ProfileName};

    crate::testutil::register_names(&["cl1"]);
    crate::testutil::write_codex_roster(&["cx1"]);
    let now = crate::usage::now_epoch_secs();
    let mut info = crate::usage::map_codex_usage(
        r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_after_seconds":7230},"secondary_window":{"used_percent":12,"limit_window_seconds":604800,"reset_after_seconds":360000}},"rate_limit_reset_credits":{"available_count":2}}"#,
        now,
    )
    .expect("maps");
    info.fetched_at = Some(crate::usage::now_ms() - 7_200_000);
    crate::profile_cache::write_profile_cache(
        &ProfileName::from("cx1"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &info,
    );

    let name = ProfileName::from("cl1");
    let profile = crate::testutil::blank_profile(&name);
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    let now = now as u64;
    app.shunt_pool = crate::tui::app::ShuntPool::Read(vec![PoolProvider {
        name: "claude-main".to_string(),
        auth: PoolAuth::Claude,
        accounts: vec![
            PoolAccount {
                plan: Some("max".to_string()),
                utilization_5h: Some(0.73),
                reset_5h: Some(now + 2 * 3600 + 13 * 60),
                utilization_7d: Some(0.41),
                reset_7d: Some(now + 4 * 86_400 + 2 * 3600),
                utilization_7d_oi: Some(0.18),
                reset_7d_oi: Some(now + 4 * 86_400 + 2 * 3600),
                ..t6_pool_account("alpha")
            },
            PoolAccount {
                available: false,
                cooldown_secs_remaining: Some(240),
                ..t6_pool_account("beta")
            },
        ],
    }]);
    app.shunt_pool_read_at_ms = Some(crate::usage::now_ms() - 120_000);
    app.tab = crate::tui::app::Tab::Usage;
    app
}

/// The Usage body drawn at 100x24, split into its two panes' inner text rows
/// (the detail pane opens at the second `╭` of the top border), each row
/// stripped of border and scrollbar cells and trimmed, trailing blank rows
/// dropped.
fn t6_panes(app: &App) -> (Vec<String>, Vec<String>) {
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
    term.draw(|f| draw(f, f.area(), app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let top: Vec<char> = rows[0].chars().collect();
    let split = top
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == '╭')
        .nth(1)
        .map(|(i, _)| i)
        .expect("a selector and a detail pane");
    let strip = |cells: &[char]| -> String {
        cells
            .iter()
            .collect::<String>()
            .trim_matches(|c: char| c == '│' || c == '┊' || c == '┃' || c.is_whitespace())
            .to_string()
    };
    let mut left = Vec::new();
    let mut right = Vec::new();
    for row in &rows[1..rows.len() - 1] {
        let cells: Vec<char> = row.chars().collect();
        left.push(strip(&cells[..split]));
        right.push(strip(&cells[split..]));
    }
    for pane in [&mut left, &mut right] {
        while pane.last().is_some_and(|r| r.is_empty()) {
            pane.pop();
        }
    }
    (left, right)
}

fn t6_down(app: &mut App) {
    crate::tui::app::handle_key(
        app,
        crate::testutil::key(ratatui::crossterm::event::KeyCode::Down),
    );
}

/// The Usage selector is Setup's band list, unfiltered: the claude rows, then
/// the `codex` band, then one `shunt (<provider>)` band per pool provider, and
/// no `+ new` row.
#[test]
fn the_usage_selector_lists_codex_and_pool_rows_in_their_bands() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = t6_app();
    let (selector, _) = t6_panes(&app);
    assert_eq!(
        selector,
        [
            "❯ cl1",
            "",
            "codex",
            "cx1",
            "",
            "shunt (claude-main)",
            "alpha",
            "beta",
        ],
        "one band walk, headings never selectable, no + new"
    );
}

/// ↓ walks the item rows only (headings and spacers skipped), and wraps from
/// the last pool row back to the first claude row, whose pane is the claude
/// one it always was.
#[test]
fn the_usage_cursor_walks_every_band_and_wraps() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = t6_app();
    let selected = |app: &App| {
        let (selector, _) = t6_panes(app);
        selector
            .into_iter()
            .find(|row| row.starts_with('❯'))
            .unwrap_or_else(|| panic!("no caret row"))
    };
    assert_eq!(selected(&app), "❯ cl1");
    t6_down(&mut app);
    assert_eq!(selected(&app), "❯ cx1");
    t6_down(&mut app);
    assert_eq!(selected(&app), "❯ alpha");
    t6_down(&mut app);
    assert_eq!(selected(&app), "❯ beta");
    t6_down(&mut app);
    assert_eq!(
        selected(&app),
        "❯ cl1",
        "↓ on the last row wraps to the first"
    );
    assert_eq!(app.profile_cursor.read(), 0);
    let (_, detail) = t6_panes(&app);
    assert!(
        detail.iter().any(|row| row.starts_with("plan"))
            && !detail.iter().any(|row| row.starts_with("type")),
        "a claude row keeps the claude pane: {detail:?}"
    );
}

/// A codex account's pane: its facts, the read age off the codex poll's own
/// cache, then the 5h and 7d bars; none of the claude-only rows.
#[test]
fn the_usage_codex_pane_shows_its_facts_and_two_windows() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = t6_app();
    t6_down(&mut app);
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        detail[..5],
        [
            "type      codex",
            "plan      plus",
            "resets    2 available",
            "status    ◌ read 2h ago",
            "",
        ],
        "facts, then a blank row before the bars: {detail:?}"
    );
    assert!(
        detail[5].starts_with("5h") && detail[5].ends_with("42%"),
        "the 5h eyebrow: {detail:?}"
    );
    assert!(
        detail[6].contains('█') || detail[6].contains('░'),
        "the 5h bar: {detail:?}"
    );
    assert_eq!(detail[7], "", "a blank between the two windows: {detail:?}");
    assert!(
        detail[8].starts_with("7d") && detail[8].ends_with("12%"),
        "the 7d eyebrow: {detail:?}"
    );
    assert!(
        detail[9].contains('█') || detail[9].contains('░'),
        "the 7d bar: {detail:?}"
    );
    assert_eq!(detail.len(), 10, "nothing after the 7d bar: {detail:?}");
}

/// The claude pane's `resets` rows, whitespace runs folded to one space so
/// the pin reads the words, not the key column's width.
fn claude_resets_rows(detail: &[String]) -> Vec<String> {
    detail
        .iter()
        .filter(|r| r.starts_with("resets"))
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// `ends` as the resets line dates it: the local month and day.
fn local_day(ends_secs: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_opt(ends_secs, 0)
        .single()
        .expect("a local instant")
        .format("%b %-d")
        .to_string()
}

/// A claude account's pane carries its banked resets off the hourly side
/// read: the usable count, the windows the next grant clears and its end;
/// a reading under 75 min old carries no age.
#[test]
fn the_usage_claude_pane_shows_its_banked_resets() {
    let _home = crate::testutil::HomeSandbox::new();
    let ends = crate::usage::now_epoch_secs() + 20 * 86_400;
    let ends_iso = chrono::DateTime::from_timestamp(ends, 0)
        .expect("instant")
        .to_rfc3339();
    crate::testutil::write_claude_limit_resets(
        "cl1",
        1,
        &["five_hour", "seven_day"],
        Some(&ends_iso),
        crate::usage::now_ms() - 10 * 60_000,
    );
    let app = t6_app();
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        claude_resets_rows(&detail),
        [format!(
            "resets 1 available · resets 5h, 7d · expires {}",
            local_day(ends)
        )],
        "{detail:?}"
    );
}

/// The window Fable counts into reads `7d fable` in the resets row, as the
/// pane's own bar names it.
#[test]
fn the_claude_resets_row_names_the_fable_weekly_7d_fable() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    crate::testutil::write_claude_limit_resets_banked(
        "cl1",
        1,
        1,
        &["seven_day_overage_included"],
        now,
    );
    let (_, detail) = t6_panes(&t6_app());
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 1 available · resets 7d fable"],
        "{detail:?}"
    );
}

/// A reading that carries the banked total says how many of them a spend can
/// use now, and keeps the row at 0 so a cooldown hides no banked reset.
#[test]
fn the_usage_claude_pane_counts_available_resets_against_the_banked_total() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    crate::testutil::write_claude_limit_resets_banked("cl1", 2, 3, &["five_hour"], now);
    let (_, detail) = t6_panes(&t6_app());
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 2/3 available · resets 5h"],
        "{detail:?}"
    );

    crate::testutil::write_claude_limit_resets_banked("cl1", 0, 3, &[], now);
    let (_, detail) = t6_panes(&t6_app());
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 0/3 available"],
        "{detail:?}"
    );

    // Every banked reset usable: the total adds nothing, so the row reads the
    // bare count.
    crate::testutil::write_claude_limit_resets_banked("cl1", 2, 2, &["five_hour"], now);
    let (_, detail) = t6_panes(&t6_app());
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 2 available · resets 5h"],
        "{detail:?}"
    );

    crate::testutil::write_claude_limit_resets_banked("cl1", 0, 0, &[], now);
    let (_, detail) = t6_panes(&t6_app());
    assert!(claude_resets_rows(&detail).is_empty(), "{detail:?}");
}

/// A reading older than 75 min (a missed hourly read) says its age.
#[test]
fn a_stale_claude_resets_reading_says_its_age() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_claude_limit_resets(
        "cl1",
        2,
        &["five_hour"],
        None,
        crate::usage::now_ms() - (3 * 3600 + 5 * 60) * 1000,
    );
    let app = t6_app();
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 2 available · resets 5h · read 3h ago"],
        "{detail:?}"
    );
}

/// No usable reset on a reading that carries no banked total, or no reading at
/// all: no `resets` row on a claude pane.
#[test]
fn a_claude_pane_without_a_usable_reset_has_no_resets_row() {
    for usable in [Some(0), None] {
        let _home = crate::testutil::HomeSandbox::new();
        if let Some(n) = usable {
            crate::testutil::write_claude_limit_resets(
                "cl1",
                n,
                &["five_hour"],
                None,
                crate::usage::now_ms() - 60_000,
            );
        }
        let app = t6_app();
        let (_, detail) = t6_panes(&app);
        assert!(
            claude_resets_rows(&detail).is_empty(),
            "usable {usable:?}: {detail:?}"
        );
    }
}

/// A quarantined codex account names it on a `login` row between `plan` and
/// `resets`; without the quarantine the row is gone (the pane pin above).
#[test]
fn a_quarantined_codex_account_shows_its_login_row() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_store(
        "cx1",
        &crate::testutil::codex_auth_body("acc.1", "refresh.cx1"),
    );
    crate::codex_auth::quarantine_for_test("cx1", "reused", "refresh.cx1");
    let mut app = t6_app();
    t6_down(&mut app);
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        detail[..4],
        [
            "type      codex",
            "plan      plus",
            "login     [ needs re-login ]",
            "resets    2 available",
        ],
        "{detail:?}"
    );
}

/// A pool account's pane: type, family, provider, plan, the listing's read
/// age, then the 5h, 7d and `7d fable` bars from its own admin row.
#[test]
fn the_usage_pool_pane_shows_its_facts_and_three_windows() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = t6_app();
    t6_down(&mut app);
    t6_down(&mut app);
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        detail[..6],
        [
            "type      shunt",
            "family    claude",
            "provider  claude-main",
            "plan      max",
            "read      2m ago",
            "",
        ],
        "an available account has no status row: {detail:?}"
    );
    for (row, label, pct) in [(6, "5h", "73%"), (9, "7d", "41%"), (12, "7d fable", "18%")] {
        assert!(
            detail[row].starts_with(label) && detail[row].ends_with(pct),
            "the {label} eyebrow at row {row}: {detail:?}"
        );
        assert!(
            detail[row + 1].contains('█') || detail[row + 1].contains('░'),
            "the {label} bar: {detail:?}"
        );
    }
    assert_eq!(detail.len(), 14, "nothing after the fable bar: {detail:?}");
}

/// A cooling-down pool account carries one `status` pill before its read age.
#[test]
fn a_cooling_down_pool_account_shows_its_status_pill() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = t6_app();
    for _ in 0..3 {
        t6_down(&mut app);
    }
    let (_, detail) = t6_panes(&app);
    assert_eq!(detail[3], "plan      —", "{detail:?}");
    assert_eq!(
        detail[4], "status    [ cooldown ]  4m",
        "the cooldown pill and its 240 s countdown: {detail:?}"
    );
    assert_eq!(detail[5], "read      2m ago", "{detail:?}");
}

/// Pool account `alpha` as shunt v0.61.0+ serializes its row: the request
/// counters ride it as top-level keys, `requests_attempted` the sum of the
/// other three since shunt's process start or the account's add,
/// `mean_latency_ms` a float that is
/// `null` until an attempt got response headers (shunt `tests/admin_surface.rs`
/// at v0.64.0). Parsed through the listing's own `Deserialize`. A 5h reading
/// keeps the bars below the facts, so the blank row after them stays inside
/// the pane's trimmed rows.
fn counted_alpha(
    attempted: u64,
    failed: u64,
    cancelled: u64,
    mean_latency_ms: Option<f64>,
) -> crate::gateway_admin::PoolAccount {
    let row = serde_json::json!({
        "name": "alpha",
        "available": true,
        "near_quota": false,
        "cooldown_secs_remaining": null,
        "needs_relogin": false,
        "paused": false,
        "disabled": false,
        "utilization_5h": 0.73,
        "reset_5h": crate::usage::now_epoch_secs() as u64 + 2 * 3600 + 13 * 60,
        "utilization_7d": null,
        "reset_7d": null,
        "utilization_7d_oi": null,
        "reset_7d_oi": null,
        "status": null,
        "requests_attempted": attempted,
        "requests_succeeded": attempted - failed - cancelled,
        "requests_failed": failed,
        "requests_cancelled": cancelled,
        "mean_latency_ms": mean_latency_ms,
    });
    serde_json::from_value(row).expect("a v0.61.0 pool row parses")
}

/// `t6_app` with `alpha` swapped for `account`, its Usage pane selected; the
/// detail pane's rows and its outer width at 100 columns.
fn counted_pool_pane(account: crate::gateway_admin::PoolAccount) -> (Vec<String>, usize) {
    let mut app = t6_app();
    let crate::tui::app::ShuntPool::Read(providers) = &mut app.shunt_pool else {
        panic!("t6_app reads its pool");
    };
    providers[0].accounts[0] = account;
    t6_down(&mut app);
    t6_down(&mut app);
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
    term.draw(|f| draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    let top: Vec<char> = crate::testutil::buffer_rows(term.backend().buffer())[0]
        .chars()
        .collect();
    let split = top
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == '╭')
        .nth(1)
        .map(|(i, _)| i)
        .expect("a selector and a detail pane");
    (t6_panes(&app).1, top.len() - split)
}

/// The counters take one `requests` row under the read age: the attempted
/// count, then the failed and cancelled counts, then the mean latency in whole
/// milliseconds (839.6 rounds to 840), counts grouped by thousands as a detail
/// pane's are.
#[test]
fn a_pool_pane_shows_the_request_counters_under_its_read_age() {
    let _home = crate::testutil::HomeSandbox::new();
    let (detail, _) = counted_pool_pane(counted_alpha(1204, 12, 3, Some(839.6)));
    assert_eq!(
        detail
            .iter()
            .take(7)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "type      shunt",
            "family    claude",
            "provider  claude-main",
            "plan      —",
            "read      2m ago",
            "requests  1,204 · 12 failed · 3 cancelled · 840 ms avg",
            "",
        ],
        "{detail:?}"
    );
}

/// A zero failed or cancelled count leaves its part out, and so does a latency
/// shunt has not measured yet; the attempted count stays, at 0 too.
#[test]
fn a_pool_pane_leaves_out_zero_counts_and_an_unmeasured_latency() {
    let _home = crate::testutil::HomeSandbox::new();
    for (account, row) in [
        (
            counted_alpha(7, 0, 0, Some(52.0)),
            "requests  7 · 52 ms avg",
        ),
        (
            counted_alpha(9, 0, 2, Some(1.0)),
            "requests  9 · 2 cancelled · 1 ms avg",
        ),
        (counted_alpha(5, 5, 0, None), "requests  5 · 5 failed"),
        (counted_alpha(0, 0, 0, None), "requests  0"),
    ] {
        let (detail, _) = counted_pool_pane(account);
        assert_eq!(
            detail
                .iter()
                .skip(4)
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["read      2m ago", row, ""],
            "{detail:?}"
        );
    }
}

/// A row too wide for the pane sheds whole parts from the right, never cuts
/// one: here the latency goes and the cancelled count stays.
#[test]
fn a_pool_pane_sheds_request_parts_whole_from_the_right() {
    let _home = crate::testutil::HomeSandbox::new();
    let (detail, pane_width) =
        counted_pool_pane(counted_alpha(1_204_567, 12_345, 3_456, Some(12_345.0)));
    // The whole row is 69 cells and the row without its latency 53, so a pane
    // whose content width (its outer width less two border and two padding
    // cells) lies in 53..69 must shed the latency part alone.
    let content = pane_width - 4;
    assert!(
        (53..69).contains(&content),
        "the fixture's pane must force exactly one shed: {content} cells"
    );
    assert_eq!(
        detail.get(5).map(String::as_str),
        Some("requests  1,204,567 · 12,345 failed · 3,456 cancelled"),
        "{detail:?}"
    );
}

/// A mean latency of a second or more is grouped by thousands like the counts.
#[test]
fn a_pool_pane_groups_a_long_latency_by_thousands() {
    let _home = crate::testutil::HomeSandbox::new();
    let (detail, _) = counted_pool_pane(counted_alpha(1204, 0, 0, Some(12_345.4)));
    assert_eq!(
        detail.get(5).map(String::as_str),
        Some("requests  1,204 · 12,345 ms avg"),
        "{detail:?}"
    );
}

/// The `requests` row's key and value text and their styles, off the row
/// builder at a given content width.
fn requests_row(account: &crate::gateway_admin::PoolAccount, inner_w: u16) -> (String, String) {
    let line = pool_requests_line(account, inner_w).expect("a counted row draws");
    assert_eq!(line.spans.len(), 2, "{line:?}");
    assert_eq!(line.spans[0].style, theme::label(), "the key: {line:?}");
    assert_eq!(line.spans[1].style, theme::accent(), "the value: {line:?}");
    (
        line.spans[0].content.to_string(),
        line.spans[1].content.to_string(),
    )
}

/// The row sheds one whole part at a time as the content width falls, keeps a
/// value that fits to the cell, and never sheds the attempted count. The full
/// value is 44 cells, 31 without the latency, 17 without the cancelled count;
/// the key column is 10.
#[test]
fn the_requests_row_sheds_one_part_per_missing_width_down_to_the_count() {
    let account = counted_alpha(1204, 12, 3, Some(839.6));
    for (inner_w, value) in [
        (54, "1,204 · 12 failed · 3 cancelled · 840 ms avg"),
        (53, "1,204 · 12 failed · 3 cancelled"),
        (41, "1,204 · 12 failed · 3 cancelled"),
        (40, "1,204 · 12 failed"),
        (27, "1,204 · 12 failed"),
        (26, "1,204"),
        (12, "1,204"),
    ] {
        assert_eq!(
            requests_row(&account, inner_w),
            ("requests  ".to_string(), value.to_string()),
            "at {inner_w} cells"
        );
    }
}

/// A mean latency that is negative or not finite is no measurement: the row
/// reads as if shunt had measured none.
#[test]
fn the_requests_row_leaves_out_a_latency_that_is_no_measurement() {
    for ms in [-123.0, -0.4, -0.0, f64::NAN, f64::INFINITY] {
        let account = crate::gateway_admin::PoolAccount {
            mean_latency_ms: Some(ms),
            ..counted_alpha(7, 0, 0, None)
        };
        assert_eq!(
            requests_row(&account, 66),
            ("requests  ".to_string(), "7".to_string()),
            "a mean of {ms}"
        );
    }
}

/// A measured mean under half a millisecond, zero included, is still a
/// measurement: it draws `0 ms avg`.
#[test]
fn the_requests_row_keeps_a_sub_millisecond_latency() {
    for ms in [0.0, 0.4] {
        let account = crate::gateway_admin::PoolAccount {
            mean_latency_ms: Some(ms),
            ..counted_alpha(7, 0, 0, None)
        };
        assert_eq!(
            requests_row(&account, 66),
            ("requests  ".to_string(), "7 · 0 ms avg".to_string()),
            "a mean of {ms}"
        );
    }
}

/// The footer offers `n note` on a claude row only: codex and pool accounts
/// keep no notes.
#[test]
fn the_usage_footer_offers_notes_on_claude_rows_only() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = t6_app();
    let footer = |app: &App| {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
        term.draw(|f| crate::tui::render::draw(f, app))
            .expect("draw");
        crate::testutil::buffer_rows(term.backend().buffer())
            .pop()
            .expect("a footer row")
    };
    assert!(
        footer(&app).contains("n note"),
        "claude row: {}",
        footer(&app)
    );
    t6_down(&mut app);
    assert!(
        !footer(&app).contains("note"),
        "codex row: {}",
        footer(&app)
    );
    t6_down(&mut app);
    assert!(!footer(&app).contains("note"), "pool row: {}", footer(&app));
}

#[test]
fn the_pace_marker_turns_danger_once_the_fill_covers_its_column() {
    let fill = Style::default().fg(theme::warning_color());
    let marker = |spans: &[Span<'_>]| {
        spans
            .iter()
            .find(|s| s.content == "│")
            .map(|s| s.style.fg)
            .expect("the marker renders")
    };
    // 6 of 10 cells filled, marker in column 3: covered, so DANGER over the fill.
    assert_eq!(
        marker(&bar_spans(6, 10, fill, Some(3))),
        Some(theme::danger_color())
    );
    // marker in column 6, the first empty cell: not reached, so it stays dim.
    assert_eq!(
        marker(&bar_spans(6, 10, fill, Some(6))),
        Some(theme::text_dim_color())
    );
}

#[test]
fn a_switching_rows_usage_spinner_turns_orange() {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let header = HeaderState {
        activity: ProfileActivity::Switching,
        next_refresh_ms: None,
        tick: 0,
        rate_limit_source: super::RateLimitSource::ThirdParty,
        streaks: StreakCounts::default(),
        kick_block: None,
        queue_slot: None,
        diag: DiagFlags::default(),
        peak: None,
        fetches_here: true,
    };
    let lines = status_lines(&profile, &header, 120, &ReadingsSnapshot::default());
    let spinner = lines[0]
        .spans
        .iter()
        .find(|s| {
            crate::spinner::SPINNER_FRAMES
                .iter()
                .any(|f| s.content.contains(f))
        })
        .unwrap_or_else(|| {
            let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
            panic!("a switching row spins: {text}")
        });
    assert_eq!(spinner.style.fg, Some(theme::accent_2_color()));
}

/// The `speed` row's fifth window is the account's peak speed today (the feed's
/// `peaks.today.accounts.<name>.speed`); the `rate` row carries no peak. The
/// peak window sheds first: at 68 columns the rate row's four windows fit (67
/// cells) but the speed row with its peak (70 cells) does not, so the speed row
/// keeps `today` and drops `peak` whole.
#[test]
fn the_speed_row_ends_in_the_accounts_peak_speed_today() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();
    let profile = Profile::new("uwuclxdy".to_string(), None, None);
    let config = AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile.clone()],
    };
    let mut app = App::new(config);
    let mut snap = tps_full_snapshot(1_000);
    snap.peaks = serde_json::from_value(serde_json::json!({
        "today": {
            "speed": {"value": 612.0, "at": "2026-09-22T21:14:00.000+00:00", "account": "kerry", "model": "claude-opus-4-8"},
            "rate": {"value": 1803.4, "at": "2026-09-22T21:02:00.000+00:00"},
            "accounts": {
                "uwuclxdy": {
                    "speed": {"value": 412.3, "at": "2026-09-22T21:10:00.000+00:00", "account": "uwuclxdy", "model": "claude-opus-4-8"},
                    "rate": {"value": 950.0, "at": "2026-09-22T21:02:00.000+00:00"}
                }
            }
        }
    }))
    .unwrap();
    app.set_tps(Some(snap));

    let rows = tps_rows(&profile, &app, 100);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live 92 t/s · 5m 88 t/s · 1h 84 t/s · today — · peak 412 t/s"
    );
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live 67 tok/s · 5m 61 tok/s · 1h 58 tok/s · today 0 tok/s"
    );

    let rows = tps_rows(&profile, &app, 68);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live 92 t/s · 5m 88 t/s · 1h 84 t/s · today —"
    );
    // The speed row with its peak is exactly 70 cells: it fits 70, not 69.
    assert_eq!(
        tps_line_text(&tps_rows(&profile, &app, 70)[0]),
        "speed     live 92 t/s · 5m 88 t/s · 1h 84 t/s · today — · peak 412 t/s"
    );
    assert_eq!(
        tps_line_text(&tps_rows(&profile, &app, 69)[0]),
        "speed     live 92 t/s · 5m 88 t/s · 1h 84 t/s · today —"
    );
    assert_eq!(
        tps_line_text(&rows[1]),
        "rate      live 67 tok/s · 5m 61 tok/s · 1h 58 tok/s · today 0 tok/s"
    );
}

/// A cache past its freshness window shows no peak, even when it carries one:
/// the speed row's `peak` window reads `—` like every other window.
#[test]
fn a_stale_snapshot_shows_no_usage_peak() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();
    let profile = Profile::new("uwuclxdy".to_string(), None, None);
    let config = AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile.clone()],
    };
    let mut app = App::new(config);
    let mut snap = tps_full_snapshot(10 * 60 * 1000);
    snap.peaks = serde_json::from_value(serde_json::json!({
        "today": {"accounts": {"uwuclxdy": {
            "speed": {"value": 412.3, "at": "2026-09-22T21:10:00.000+00:00", "account": "uwuclxdy", "model": "claude-opus-4-8"}
        }}}
    }))
    .unwrap();
    app.set_tps(Some(snap));
    assert!(app.fresh_tps().is_none(), "a 10-minute-old cache is stale");
    let rows = tps_rows(&profile, &app, 100);
    assert_eq!(
        tps_line_text(&rows[0]),
        "speed     live — · 5m — · 1h — · today — · peak —"
    );
}

/// The notes row is a static key/value row: its key carries no colon and
/// renders dim + bold like every other static key, with or without a note.
#[test]
fn the_notes_key_is_a_colonless_static_key() {
    for note in [None, Some("first line")] {
        let lines = notes_lines(note, 60);
        let key = &lines[0].spans[0];
        assert_eq!(key.content, "notes     ", "note {note:?}");
        assert_eq!(key.style, theme::label(), "note {note:?}");
    }
}

/// A third-party stat row's key is a static key: dim + bold, never faint.
#[test]
fn a_third_party_stat_key_renders_dim_and_bold() {
    let spans = key_value_span("remaining", "$4.20", Style::default(), 8);
    assert_eq!(spans[0].content.trim(), "remaining");
    assert_eq!(spans[0].style, theme::label());
}

/// A hybrid profile (a claude OAuth pair beside a provider endpoint) with a
/// reading shows the `resets` row: the file's presence is the one predicate,
/// the same one the Overview chip reads, so the pane never narrows it to
/// pure-OAuth accounts.
#[test]
fn a_hybrid_profile_with_a_reading_shows_the_resets_row() {
    use crate::profile::{AppConfig, AppState, ProfileName};

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["cl1"]);
    crate::testutil::write_claude_limit_resets(
        "cl1",
        1,
        &["five_hour"],
        None,
        crate::usage::now_ms() - 60_000,
    );
    let mut profile = crate::profile::Profile::new(
        "cl1".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-hybrid".to_string()),
    );
    profile.credentials = Some(crate::testutil::oauth_pair("cl1"));
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("cl1")],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    app.tab = crate::tui::app::Tab::Usage;
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 1 available · resets 5h"],
        "{detail:?}"
    );
}

/// The claude `resets` row sits right under the plan row — the pane's first
/// two rows — wherever the other rows fall.
#[test]
fn the_claude_resets_row_sits_right_under_the_plan_row() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_claude_limit_resets(
        "cl1",
        2,
        &["five_hour"],
        None,
        crate::usage::now_ms() - 60_000,
    );
    let app = t6_app();
    let (_, detail) = t6_panes(&app);
    assert!(
        detail[0].starts_with("plan"),
        "the plan row leads the pane: {detail:?}"
    );
    assert!(
        detail[1].starts_with("resets"),
        "the resets row sits right under the plan row: {detail:?}"
    );
}

/// The age part appears only once the reading is older than 75 min: 74 min
/// shows no `· read` part, 76 min says `1h`. The code compares strictly
/// (`>`), so exactly 75 min is not aged; the renderer reads the real clock,
/// so the pins sit one minute off the boundary instead of on it.
#[test]
fn the_resets_row_says_its_age_only_past_75_minutes() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_claude_limit_resets(
        "cl1",
        1,
        &[],
        None,
        crate::usage::now_ms() - 74 * 60_000,
    );
    let app = t6_app();
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 1 available"],
        "74 min: no age part: {detail:?}"
    );
    crate::testutil::write_claude_limit_resets(
        "cl1",
        1,
        &[],
        None,
        crate::usage::now_ms() - 76 * 60_000,
    );
    let app = t6_app();
    let (_, detail) = t6_panes(&app);
    assert_eq!(
        claude_resets_rows(&detail),
        ["resets 1 available · read 1h ago"],
        "76 min: the age part: {detail:?}"
    );
}

/// A row too wide for its pane ends on `…`: a bare clip turned `expires Nov 26`
/// into a false `expires Nov 2`. At 57 columns the pane holds 53 cells, 43 of
/// them after the key, one short of this 44-cell value. A row that fits keeps
/// every part, the exact fit included.
#[test]
fn a_claude_resets_row_past_its_pane_says_it_was_cut() {
    use chrono::TimeZone as _;

    fn row_at(app: &App, width: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30))
            .expect("terminal");
        term.draw(|f| draw(f, f.area(), app, &app.readings_snapshot(), &[]))
            .expect("draw");
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let at = rows
            .iter()
            .find(|r| r.contains("resets  "))
            .unwrap_or_else(|| panic!("a resets row at {width}: {rows:?}"));
        let tail = &at[at.find("resets  ").expect("the row")..];
        let end = tail.find(['│', '┊', '┃']).unwrap_or(tail.len());
        tail[..end].split_whitespace().collect::<Vec<_>>().join(" ")
    }

    let _home = crate::testutil::HomeSandbox::new();
    let ends = chrono::Local
        .with_ymd_and_hms(2099, 11, 26, 12, 0, 0)
        .single()
        .expect("a local instant")
        .to_rfc3339();
    crate::testutil::write_claude_limit_resets(
        "cl1",
        1,
        &["five_hour", "seven_day"],
        Some(&ends),
        crate::usage::now_ms() - 10 * 60_000,
    );
    let app = t6_app();

    assert_eq!(
        row_at(&app, 57),
        "resets 1 available · resets 5h, 7d · expires Nov …"
    );
    assert_eq!(
        row_at(&app, 58),
        "resets 1 available · resets 5h, 7d · expires Nov 26",
        "44 cells in a 44-cell budget fit"
    );
}

/// A quarantined proxy account's status block sends it to the login through
/// its proxy, the one login clauth takes for it; the same account with no
/// proxy record keeps the re-login fix, byte for byte.
#[test]
fn a_quarantined_proxy_accounts_status_names_the_login_through_its_proxy() {
    let rows = |proxy: bool| -> Vec<String> {
        let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("px"));
        profile.proxy = proxy.then(|| crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        });
        let header = HeaderState {
            diag: DiagFlags {
                auth_broken: true,
                ..DiagFlags::default()
            },
            ..idle_header()
        };
        status_lines(&profile, &header, 120, &ReadingsSnapshot::default())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    };
    assert_eq!(
        (rows(true), rows(false)),
        (
            vec![
                "status    [ auth broken ]".to_string(),
                "└ log in again with clauth login px --proxy stub".to_string(),
            ],
            vec![
                "status    [ auth broken ]".to_string(),
                "└ re-login with clauth login px".to_string(),
            ],
        ),
        "(the proxy account's status rows, the plain account's)"
    );
}

/// A keyless proxy account holding a stored pair loads as an OAuth login no
/// leg polls; its empty Usage body names the login through its proxy, while a
/// plain account holding nothing keeps the setup-tab line.
#[test]
fn a_keyless_proxy_account_holding_a_pair_names_its_login_in_the_empty_body() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px", "oau"]);
    let name = ProfileName::from("px");
    let mut px = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
    );
    px.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-px".to_string(),
            refresh_token: Some("rt-px".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&px).expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    let px = crate::profile::load_profile(&name).expect("load profile");
    assert_eq!(
        (
            px.proxy.is_some(),
            px.is_oauth(),
            px.access_token(),
            px.usage_cache_is_third_party(),
        ),
        (true, true, Some("at-px"), false),
        "fixture control: (the proxy record, an OAuth body, the stored pair, a third-party body)"
    );
    let rows = |profile: crate::profile::Profile| -> Vec<String> {
        let mut app = App::new(AppConfig {
            state: AppState {
                profiles: vec![profile.name.clone()],
                ..AppState::default()
            },
            profiles: vec![profile],
        });
        app.profile_cursor.set(0);
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
            .expect("draw");
        crate::testutil::buffer_rows(term.backend().buffer())
    };
    let proxy_row = format!(
        "│   no api key set, run `clauth login px --proxy stub`{}│",
        " ".repeat(25)
    );
    let plain_row = format!(
        "│   not logged in, use + login on the setup tab{}│",
        " ".repeat(32)
    );

    let proxy_rows = rows(px);
    let plain_rows = rows(crate::testutil::blank_profile(&ProfileName::from("oau")));
    assert_eq!(
        (
            proxy_rows.contains(&proxy_row),
            plain_rows.contains(&plain_row)
        ),
        (true, true),
        "(the proxy account's empty body row, the plain account's): {proxy_rows:#?} {plain_rows:#?}"
    );
}

/// A stored OAuth pair for `name`, the access token `at-<name>`.
fn stored_pair(name: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// Keyless proxy account `px` of proxy `stub` holding a stored pair, saved
/// and loaded back from disk, with `auto_start` as given and its record's
/// `account` set to `account` (an empty one is a table clauth cannot read).
fn keyless_proxy_account_holding_a_pair(
    auto_start: bool,
    account: &str,
) -> crate::profile::Profile {
    let name = crate::profile::ProfileName::from("px");
    let mut px = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
    );
    px.auto_start = auto_start;
    px.credentials = Some(stored_pair("px"));
    crate::profile::save_profile(&px).expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &crate::profile::ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    let path = crate::profile::profile_subpath(&name, "config.toml").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text.replace("account = \"acct-1\"", &format!("account = \"{account}\""));
    assert_eq!(
        edited == text,
        account == "acct-1",
        "fixture control: the record's account is {account:?}"
    );
    std::fs::write(&path, edited).unwrap();
    crate::profile::load_profile(&name).expect("load profile")
}

/// `draw_usage_detail`'s rows for `profile` alone, on a `width`x24 frame with
/// no readings.
fn usage_detail_rows(profile: crate::profile::Profile, width: u16) -> Vec<String> {
    use crate::profile::{AppConfig, AppState};
    use crate::tui::app::App;

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    app.profile_cursor.set(0);
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).expect("terminal");
    term.draw(|f| draw_usage_detail(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .expect("draw");
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// A profile holding a stored pair whose `[proxy]` table clauth cannot read
/// loads as an OAuth login no leg polls and every login refuses; its empty
/// Usage body says to fix the table, while a plain account holding nothing
/// keeps the setup-tab line.
#[test]
fn an_unreadable_proxy_record_holding_a_pair_names_the_table_in_the_empty_body() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px", "oau"]);
    let px = keyless_proxy_account_holding_a_pair(false, "");
    assert_eq!(
        (
            px.proxy.is_some(),
            px.has_unreadable_proxy_record,
            px.is_oauth(),
            px.access_token(),
        ),
        (false, true, true, Some("at-px")),
        "fixture control: (a readable proxy record, an unreadable one, an OAuth body, the \
         stored pair)"
    );
    let unreadable_row = format!(
        "│   can't read its [proxy] table, fix it in config.toml{}│",
        " ".repeat(24)
    );
    let plain_row = format!(
        "│   not logged in, use + login on the setup tab{}│",
        " ".repeat(32)
    );

    let px_rows = usage_detail_rows(px, 80);
    let plain_rows = usage_detail_rows(
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("oau")),
        80,
    );
    assert_eq!(
        (
            px_rows.contains(&unreadable_row),
            plain_rows.contains(&plain_row)
        ),
        (true, true),
        "(the unreadable record's empty body row, the plain account's): {px_rows:#?} \
         {plain_rows:#?}"
    );
}

/// A proxy account with `auto_start` on holding a stored pair shows no
/// auto-start countdown on its plan row, since no kick spends its chain; a
/// plain OAuth account with `auto_start` on shows it.
#[test]
fn a_proxy_account_shows_no_auto_start_countdown() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px", "oa"]);
    let px = keyless_proxy_account_holding_a_pair(true, "acct-1");
    let mut oa = crate::profile::Profile::new("oa".to_string(), None, None);
    oa.auto_start = true;
    oa.credentials = Some(stored_pair("oa"));
    assert_eq!(
        (
            px.proxy.is_some(),
            px.auto_start,
            px.access_token(),
            oa.auto_start,
            oa.access_token()
        ),
        (true, true, Some("at-px"), true, Some("at-oa")),
        "fixture control: (px's proxy record, its auto_start, its pair, oa's auto_start, its \
         pair)"
    );
    let px_plan_row = format!("│ plan      —{}│", " ".repeat(76));
    let oa_plan_row = format!("│ plan      —{}usage auto-start due now │", " ".repeat(51));

    let px_rows = usage_detail_rows(px, 90);
    let oa_rows = usage_detail_rows(oa, 90);
    assert_eq!(
        (
            px_rows.contains(&px_plan_row),
            px_rows.iter().any(|row| row.contains("usage auto-start")),
            oa_rows.contains(&oa_plan_row)
        ),
        (true, false, true),
        "(px's bare plan row, any countdown on px, oa's countdown row): {px_rows:#?} \
         {oa_rows:#?}"
    );
}

/// A proxy account holding a setup-token sidecar beside its stored pair, its
/// `[proxy]` table readable or not, gets its own empty-body line, never the
/// sidecar line naming `+ login`, which refuses it; a plain account holding
/// the same sidecar keeps the sidecar line.
#[test]
fn a_proxy_account_holding_a_sidecar_never_reads_the_relogin_line() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px", "oau"]);
    let with_sidecar = |mut profile: crate::profile::Profile| {
        profile.has_long_lived_sidecar = true;
        profile
    };
    let readable = with_sidecar(keyless_proxy_account_holding_a_pair(false, "acct-1"));
    let readable_shape = (
        readable.proxy.is_some(),
        readable.has_unreadable_proxy_record,
        readable.is_oauth(),
    );
    let readable_rows = usage_detail_rows(readable, 80);
    let unreadable = with_sidecar(keyless_proxy_account_holding_a_pair(false, ""));
    let unreadable_shape = (
        unreadable.proxy.is_some(),
        unreadable.has_unreadable_proxy_record,
        unreadable.is_oauth(),
    );
    let unreadable_rows = usage_detail_rows(unreadable, 80);
    let plain_rows = usage_detail_rows(
        with_sidecar(crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("oau"),
        )),
        120,
    );
    assert_eq!(
        (readable_shape, unreadable_shape),
        ((true, false, true), (false, true, true)),
        "fixture control: (a readable proxy record, an unreadable one, an OAuth body) for each"
    );
    let readable_row = format!(
        "│   no api key set, run `clauth login px --proxy stub`{}│",
        " ".repeat(25)
    );
    let unreadable_row = format!(
        "│   can't read its [proxy] table, fix it in config.toml{}│",
        " ".repeat(24)
    );
    let plain_row = format!(
        "│   can't read usage, log in the same account with + login on the setup tab, or turn \
         on its usage probe{}│",
        " ".repeat(16)
    );

    assert_eq!(
        (
            readable_rows.contains(&readable_row),
            unreadable_rows.contains(&unreadable_row),
            plain_rows.contains(&plain_row),
        ),
        (true, true, true),
        "(the readable proxy account's empty body row, the unreadable record's, the plain \
         account's): {readable_rows:#?} {unreadable_rows:#?} {plain_rows:#?}"
    );
}

/// The fetch row of a stood-down instance 4 minutes past its deadline, in
/// `status` (its pill and retry ordinal kept), with the reading 5.5 minutes
/// old so `read 5m ago` lands away from a minute boundary. Returns the token
/// vector and the row's joined span text — the token pin collapses whitespace,
/// so the spacing (the double space after a pill) needs the joined form
/// pinned beside it.
fn overdue_status_row(status: FetchStatus, rate_limit: u32) -> (Vec<String>, String) {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - 330_000),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    readings = snap_with_status(readings, "a", status);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: false,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - 240_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts {
                rate_limit,
                ..StreakCounts::default()
            },
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let row = lines
        .iter()
        .find(|l| l.contains("overdue") || l.contains("refresh in") || l.contains("retry in"))
        .unwrap_or_else(|| panic!("a fetch row renders: {lines:?}"))
        .clone();
    (row.split_whitespace().map(str::to_string).collect(), row)
}

/// `[ failed ]` overdue: the pill and verb stay, the countdown becomes the
/// overdue wording dated by the reading.
#[test]
fn a_failed_overdue_fetch_row_says_retry_overdue_and_dates_the_reading() {
    let _home = crate::testutil::HomeSandbox::new();
    let (tokens, joined) = overdue_status_row(FetchStatus::Failed, 0);
    assert_eq!(
        tokens,
        [
            "status", "[", "failed", "]", "retry", "overdue", "·", "read", "5m", "ago"
        ],
    );
    assert_eq!(
        joined, "status    [ failed ]  retry overdue · read 5m ago",
        "the double space after the pill is pinned byte-exact"
    );
}

/// `[ cached ]` overdue keeps its `refresh` verb (a refresh attempt, not a
/// retry, while the token chain is not failing).
#[test]
fn a_cached_overdue_fetch_row_says_refresh_overdue_and_dates_the_reading() {
    let _home = crate::testutil::HomeSandbox::new();
    let (tokens, joined) = overdue_status_row(FetchStatus::Cached, 0);
    assert_eq!(
        tokens,
        [
            "status", "[", "cached", "]", "refresh", "overdue", "·", "read", "5m", "ago"
        ],
    );
    assert_eq!(
        joined, "status    [ cached ]  refresh overdue · read 5m ago",
        "the double space after the pill is pinned byte-exact"
    );
}

/// `[ rate limited ]` overdue keeps the retry ordinal: the depth signal
/// survives the reworded countdown.
#[test]
fn a_rate_limited_overdue_fetch_row_keeps_the_retry_ordinal() {
    let _home = crate::testutil::HomeSandbox::new();
    let (tokens, joined) = overdue_status_row(FetchStatus::RateLimited, 3);
    assert_eq!(
        tokens,
        [
            "status", "[", "rate", "limited", "]", "3rd", "retry", "overdue", "·", "read", "5m",
            "ago"
        ],
    );
    assert_eq!(
        joined, "status    [ rate limited ]  3rd retry overdue · read 5m ago",
        "the double space after the pill is pinned byte-exact"
    );
}

/// A fetching instance with a past deadline keeps its `queued` spinner: a
/// fetch IS coming there, so the stood-down overdue wording never replaces
/// it (and `refresh in 0s` never appears either).
#[test]
fn a_fetching_instance_with_a_past_deadline_keeps_the_queued_spinner() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - 240_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &ReadingsSnapshot::default(),
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let row = lines
        .iter()
        .find(|l| l.split_whitespace().any(|w| w == "queued"))
        .unwrap_or_else(|| panic!("a queued row renders: {lines:?}"));
    assert!(
        crate::spinner::SPINNER_FRAMES
            .iter()
            .any(|f| row.split_whitespace().any(|w| w.contains(f))),
        "`queued` follows its spinner frame: {row:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("overdue")),
        "a fetching instance's past deadline never reads overdue: {lines:?}"
    );
}

/// A third-party reading's `read N ago` dates off its cache mtime, which the
/// snapshot's per-tick clock map carries (its `UsageInfo` holds no
/// `fetched_at` stamp).
#[test]
fn a_third_party_overdue_fetch_row_dates_off_its_cache_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.base_url = Some("https://example.com".to_string());
    profile.api_key = Some("key".to_string());
    let reading = crate::usage::UsageInfo::default();
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading);
    readings.clock.insert("a".to_string(), now_ms() - 330_000);
    readings.is_third_party.insert("a".to_string(), true);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: false,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - 240_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let row = lines
        .iter()
        .find(|l| l.contains("overdue"))
        .unwrap_or_else(|| panic!("the overdue row renders: {lines:?}"));
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>(),
        [
            "status", "◌", "refresh", "overdue", "·", "read", "5m", "ago"
        ],
        "the third-party reading dates off its cache mtime, no fetched_at stamp"
    );
}

/// An OAuth body with no `fetched_at` stamp shows its overdue form undated:
/// the cache mtime that dates its due time is not a stamp clauth read, so
/// no `read Nm ago` can ride the row — with and without the `[ stale ]`
/// pill, which keeps its own threshold.
#[test]
fn an_undated_overdue_fetch_row_reads_overdue_without_a_read_age() {
    let _home = crate::testutil::HomeSandbox::new();
    for stale in [false, true] {
        let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
        let mut readings = snap_with_usage(
            ReadingsSnapshot::default(),
            "a",
            crate::usage::UsageInfo::default(),
        );
        readings = snap_with_oauth_usage(readings, "a", crate::usage::UsageInfo::default());
        readings.clock.insert("a".to_string(), now_ms() - 330_000);
        if stale {
            readings = snap_with_stale(readings, "a");
        }
        let lines: Vec<String> = status_lines(
            &profile,
            &HeaderState {
                fetches_here: false,
                activity: ProfileActivity::Idle,
                next_refresh_ms: Some(now_ms() - 240_000),
                tick: 0,
                rate_limit_source: super::RateLimitSource::ThirdParty,
                streaks: StreakCounts::default(),
                kick_block: None,
                queue_slot: None,
                diag: DiagFlags::default(),
                peak: None,
            },
            120,
            &readings,
        )
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .collect();
        let row = lines
            .iter()
            .find(|l| l.contains("overdue"))
            .unwrap_or_else(|| panic!("the overdue row renders: {lines:?}"))
            .clone();
        let expected: Vec<&str> = if stale {
            vec!["status", "[", "stale", "]", "◌", "refresh", "overdue"]
        } else {
            vec!["status", "◌", "refresh", "overdue"]
        };
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            expected,
            "no stamp, no read age: the undated form (stale={stale})"
        );
    }
}

/// [`overdue_status_row`] for a body with no `fetched_at`: the due time
/// derives off the clock map's mtime, but the read age has no stamp to
/// date.
fn undated_overdue_status_row(status: FetchStatus, rate_limit: u32) -> (Vec<String>, String) {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let mut readings = snap_with_usage(
        ReadingsSnapshot::default(),
        "a",
        crate::usage::UsageInfo::default(),
    );
    readings = snap_with_oauth_usage(readings, "a", crate::usage::UsageInfo::default());
    readings.clock.insert("a".to_string(), now_ms() - 330_000);
    readings = snap_with_status(readings, "a", status);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: false,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() - 240_000),
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts {
                rate_limit,
                ..StreakCounts::default()
            },
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags::default(),
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();
    let row = lines
        .iter()
        .find(|l| l.contains("overdue") || l.contains("refresh in") || l.contains("retry in"))
        .unwrap_or_else(|| panic!("a fetch row renders: {lines:?}"))
        .clone();
    (row.split_whitespace().map(str::to_string).collect(), row)
}

/// The pill arms keep their pill, verb and retry ordinal on an undated
/// body, dropping only the ` · read Nm ago` half: the due time (the cache
/// mtime plus the interval) still reads overdue, the read age has no stamp.
#[test]
fn the_pill_arms_read_overdue_undated_when_the_body_has_no_stamp() {
    let _home = crate::testutil::HomeSandbox::new();
    let (tokens, joined) = undated_overdue_status_row(FetchStatus::Failed, 0);
    assert_eq!(tokens, ["status", "[", "failed", "]", "retry", "overdue"]);
    assert_eq!(
        joined, "status    [ failed ]  retry overdue",
        "the double space after the pill is pinned byte-exact"
    );
    let (tokens, joined) = undated_overdue_status_row(FetchStatus::Cached, 0);
    assert_eq!(tokens, ["status", "[", "cached", "]", "refresh", "overdue"]);
    assert_eq!(
        joined, "status    [ cached ]  refresh overdue",
        "the double space after the pill is pinned byte-exact"
    );
    let (tokens, joined) = undated_overdue_status_row(FetchStatus::RateLimited, 3);
    assert_eq!(
        tokens,
        [
            "status", "[", "rate", "limited", "]", "3rd", "retry", "overdue"
        ]
    );
    assert_eq!(
        joined, "status    [ rate limited ]  3rd retry overdue",
        "the double space after the pill is pinned byte-exact"
    );
}

/// A failed account whose leg a left-running fetch worker holds has no
/// published deadline: the reading's own stamp plus the account's interval
/// dates when the refresh fell due, so the fetch row reads the overdue
/// wording on the instance that fetches, never a bare `[ failed ]`.
#[test]
fn a_late_workers_failed_fetch_row_says_retry_overdue_and_dates_the_reading() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let interval = crate::usage::usage_interval_ms(
        &profile,
        crate::profile::AppState::default().refresh_interval_ms,
    );
    // The 90 s interval + 4m30s lands the stamp exactly 6 m out, and
    // `spoken_age` floors, so `read 6m ago` holds from the stamp to the 7 m
    // boundary — a full minute of margin.
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - interval - 270_000),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    readings = snap_with_status(readings, "a", FetchStatus::Failed);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                interval_ms: interval,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    let row = lines
        .iter()
        .find(|l| l.contains("overdue"))
        .unwrap_or_else(|| panic!("the overdue row renders: {lines:?}"));
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>(),
        [
            "status", "[", "failed", "]", "retry", "overdue", "·", "read", "6m", "ago"
        ],
    );
    assert_eq!(
        row, "status    [ failed ]  retry overdue · read 6m ago",
        "the double space after the pill is pinned byte-exact"
    );
}

/// A deliberately blanked leg keeps its blank slot even when its reading
/// dates an overdue refresh: the blank IS the published fact (a spent skip,
/// a suppressed credential, a selected leg on no work list), never a stuck
/// worker's silence, so the fetch row renders the bare `[ spent ]` pill.
#[test]
fn a_blanked_spent_row_stays_bare_even_with_a_reading_clock() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let interval = crate::usage::usage_interval_ms(
        &profile,
        crate::profile::AppState::default().refresh_interval_ms,
    );
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - 330_000),
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 100.0,
            resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
        }),
        ..Default::default()
    };
    let readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                blanked: true,
                interval_ms: interval,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    )
    .iter()
    .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
    .collect();

    let row = lines
        .iter()
        .find(|l| l.contains("[ spent ]"))
        .unwrap_or_else(|| panic!("the spent pill renders: {lines:?}"));
    assert_eq!(
        row.split_whitespace().collect::<Vec<_>>(),
        ["status", "[", "spent", "]"],
        "the deliberate blank keeps the pill bare"
    );
    assert!(
        !lines.iter().any(|l| l.contains("overdue")),
        "the blank, not a stuck worker, holds the leg:\n{lines:?}"
    );
}

/// The three overdue fetch-row shapes whose reading-age suffix the width
/// guard sheds: the stale-plus-pill row, the bare queue dot, and the late
/// worker's failed row.
#[derive(Clone, Copy, Debug)]
enum OverdueBoundary {
    StaleCached,
    Dot,
    LateFailed,
}

/// A stood-down fetch row in [`OverdueBoundary`]'s shape, drawn at `inner_w`.
fn overdue_boundary_lines(shape: OverdueBoundary, inner_w: u16) -> Vec<Line<'static>> {
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - 13 * 60_000 - 30_000),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    let (fetches_here, next_refresh_ms, status) = match shape {
        OverdueBoundary::StaleCached => {
            (false, Some(now_ms() - 240_000), Some(FetchStatus::Cached))
        }
        OverdueBoundary::Dot => (false, Some(now_ms() - 240_000), None),
        OverdueBoundary::LateFailed => (true, None, Some(FetchStatus::Failed)),
    };
    if let Some(status) = status {
        readings = snap_with_status(readings, "a", status);
    }
    if matches!(shape, OverdueBoundary::StaleCached) {
        readings = snap_with_stale(readings, "a");
    }
    status_lines(
        &profile,
        &HeaderState {
            fetches_here,
            activity: ProfileActivity::Idle,
            next_refresh_ms,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                interval_ms: crate::profile::DEFAULT_REFRESH_INTERVAL_MS,
                ..DiagFlags::default()
            },
            peak: None,
        },
        inner_w,
        &readings,
    )
}

/// The fetch row among `lines`, found by its overdue wording.
fn overdue_boundary_row<'a>(lines: &'a [Line<'static>]) -> &'a Line<'static> {
    lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("overdue")))
        .unwrap_or_else(|| panic!("the overdue fetch row renders: {lines:?}"))
}

/// The overdue row sheds its ` · read Nm ago` figure at its own exact display
/// width: `inner_w` equal to the row's cells keeps the figure, one cell less
/// drops it whole. A guard that sheds a cell early or late cuts a figure the
/// eye reads as data, so both sides of the boundary are pinned.
#[test]
fn the_overdue_fetch_row_sheds_its_reading_age_at_its_exact_display_width() {
    let _home = crate::testutil::HomeSandbox::new();
    for (shape, dated, undated) in [
        (
            OverdueBoundary::StaleCached,
            "status    [ stale ] [ cached ]  refresh overdue · read 13m ago",
            "status    [ stale ] [ cached ]  refresh overdue",
        ),
        (
            OverdueBoundary::Dot,
            "status    ◌ refresh overdue · read 13m ago",
            "status    ◌ refresh overdue",
        ),
        (
            OverdueBoundary::LateFailed,
            "status    [ failed ]  retry overdue · read 13m ago",
            "status    [ failed ]  retry overdue",
        ),
    ] {
        let wide = overdue_boundary_lines(shape, 200);
        let row = overdue_boundary_row(&wide);
        let text = |l: &Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(
            text(row),
            dated,
            "{shape:?}: the row fits whole at 200 cells"
        );
        let exact = spans_cells(&row.spans) as u16;
        assert_eq!(
            text(overdue_boundary_row(&overdue_boundary_lines(shape, exact))),
            dated,
            "{shape:?}: at the row's own {exact} cells the reading age stays",
        );
        assert_eq!(
            text(overdue_boundary_row(&overdue_boundary_lines(
                shape,
                exact - 1
            ))),
            undated,
            "{shape:?}: one cell short of {exact} drops the reading age whole",
        );
    }
}

/// A probed account's overdue row derives its due time from the probe
/// interval, not from the base poll interval the diag flags carry: the
/// selector runs once, inside the row.
#[test]
fn a_probed_accounts_overdue_fetch_row_derives_from_its_probe_interval() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.usage_probe = true;
    profile.has_long_lived_sidecar = true;
    profile.usage_probe_interval_ms = 90_000;
    // Two minutes on the reading is past the probe's 90 s cadence but not the
    // base 300 s one, so only the probe interval dates it overdue.
    let reading = crate::usage::UsageInfo {
        fetched_at: Some(now_ms() - 120_000),
        ..crate::usage::UsageInfo::default()
    };
    let mut readings = snap_with_usage(ReadingsSnapshot::default(), "a", reading.clone());
    readings = snap_with_oauth_usage(readings, "a", reading);
    readings = snap_with_status(readings, "a", FetchStatus::Failed);
    let lines = status_lines(
        &profile,
        &HeaderState {
            fetches_here: true,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            rate_limit_source: super::RateLimitSource::ThirdParty,
            streaks: StreakCounts::default(),
            kick_block: None,
            queue_slot: None,
            diag: DiagFlags {
                interval_ms: 300_000,
                ..DiagFlags::default()
            },
            peak: None,
        },
        120,
        &readings,
    );
    let row = overdue_boundary_row(&lines);
    assert_eq!(
        row.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>(),
        "status    [ failed ]  retry overdue · read 2m ago",
        "the probe's 90 s cadence dates the reading, not the 300 s base"
    );
}
