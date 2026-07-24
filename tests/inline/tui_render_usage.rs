use super::*;

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
    assert_eq!(stats[0].pct, 0.0);
    assert_eq!(stats[1].pct, 12.0);
}

/// A bar whose label decodes to a window (`5h`/`7d`/`30d`) gets the OAuth window
/// predictions: a window-anchored average pace (sub-day → %/h, `<n>d` → %/d) and
/// the ideal-pace marker. Toggles gate them; non-window labels stay bare.
#[test]
fn stats_from_bars_fills_pace_for_windowed_labels() {
    let approx = |a: Option<f64>, b: f64| a.is_some_and(|v| (v - b).abs() < 0.1);
    let now = crate::usage::now_epoch_secs();
    // 5h window 4h in (resets in 1h), 20% used → 5 %/h, 80% of the way through.
    // 7d window 3.5d in, 35% used → 10 %/d, half elapsed.
    // 30d window 15d in, 30% used → 2 %/d (proves the new 30d duration arm).
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
    }
}

// ── oauth empty states ────────────────────────────────────────────────────────
//
// The oauth body must not spin "loading" forever: a credential-less profile is
// never fetched (issue #2's permanent "loading"), and a Failed fetch is
// terminal. Only a still-possible fetch may show "loading".

#[test]
fn empty_msg_credless_profile_is_terminal() {
    let profile = crate::testutil::blank_profile("a");
    assert_eq!(
        oauth_empty_msg(&profile),
        "not logged in, use + login on the setup tab"
    );
}

#[test]
fn empty_msg_failed_fetch_is_terminal() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    profile.fetch_status = Some(FetchStatus::Failed);
    assert_eq!(oauth_empty_msg(&profile), "no usage available");
}

#[test]
fn empty_msg_pending_fetch_loads() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    assert_eq!(oauth_empty_msg(&profile), "loading");
}

/// A disabled profile is never scheduled (`collect_tokens` skips it), so with no
/// seeded cache a fetch never lands — the body must be terminal, not spin
/// "loading" forever. The sibling `empty_msg_pending_fetch_loads` (identical but
/// enabled → "loading") proves it's the `disabled` flag that flips the outcome.
#[test]
fn empty_msg_disabled_profile_is_terminal() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    profile.disabled = true;
    assert_eq!(oauth_empty_msg(&profile), "no usage available");
}

/// The third-party body has the same never-scheduled hole: a disabled api-key
/// profile is dropped by `collect_third_party_entries`, so it never loads and
/// must read "no usage available" instead of spinning "loading" forever.
#[test]
fn tp_rows_disabled_profile_is_terminal() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.disabled = true;
    // No `third_party_usage`, no fetch_status → the un-fixed path returns "loading".
    let rendered: Vec<String> = build_tp_rows(&profile, 52, false, false, ResetFmt::default())
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

/// With no fetched plan, the Usage `plan` row must match the Overview's tier
/// label (`endpoint_label`) instead of a bare "oauth"/"api", so the two surfaces
/// never disagree. A `subscription_type` claim renders as its tier.
#[test]
fn header_lines_plan_falls_back_to_endpoint_label() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".into()),
        }),
    });
    // No `usage`, no `third_party_usage` → the plan-label fallback is exercised.
    let header = HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: None,
        diag: DiagFlags::default(),
    };
    let plan_row: String = header_lines(&profile, &header, 52)
        .first()
        .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
        .unwrap_or_default();
    let expected = crate::format::endpoint_label(&profile);
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

/// The `endpoint_label` fallback is gated to OAuth profiles: an api-key profile
/// with no plan keeps "api", never its raw endpoint url (which `endpoint_label`
/// returns base-url-first). Pins the regression a blanket `endpoint_label` swap
/// would cause on DeepSeek/z.ai/generic rows.
#[test]
fn header_lines_plan_keeps_api_for_api_key_profiles() {
    let profile = crate::profile::Profile::new(
        "a".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let header = HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: None,
        diag: DiagFlags::default(),
    };
    let plan_row: String = header_lines(&profile, &header, 52)
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

/// The canceled pill is sourced purely from `profile.usage` (populated at
/// startup by `bootstrap_fetch`'s on-disk cache seed, see
/// `usage::scheduler::try_seed_cache`), never a live fetch. A profile carrying
/// a prior session's canceled plan must show the pill from the cached state
/// alone, before any network call.
#[test]
fn status_lines_shows_canceled_from_a_prior_sessions_cached_plan() {
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let mut profile = crate::testutil::blank_profile("a");
    profile.usage = Some(UsageInfo {
        codex_rate_limit_reached: None,
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    });
    let header = HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: None,
        diag: DiagFlags::default(),
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

    let rendered = text(status_lines(&profile, &header, 120));
    assert!(rendered.contains("canceled"), "got {rendered:?}");
}

/// Regression guard the other direction: an un-canceled cached plan never
/// paints the canceled pill.
#[test]
fn status_lines_no_canceled_pill_when_subscription_is_active() {
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let mut profile = crate::testutil::blank_profile("a");
    profile.usage = Some(UsageInfo {
        codex_rate_limit_reached: None,
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: None,
        }),
        ..Default::default()
    });
    let header = HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: None,
        diag: DiagFlags::default(),
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

    let rendered = text(status_lines(&profile, &header, 120));
    assert!(!rendered.contains("canceled"), "got {rendered:?}");
}

/// Shared fixture for the two disabled-rung tests: a header whose lower rungs
/// are all armed, so each test's assertion is about what the disabled rung does
/// to them rather than about which rung happened to fire.
fn disabled_rung_header(kick: bool) -> HeaderState {
    use crate::usage::KickBlock;
    HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: kick.then(|| KickBlock {
            streak: 3,
            rejected: true,
            until: Some(now_epoch_secs() + 3600),
            next_retry: now_epoch_secs() + 30,
        }),
        diag: DiagFlags::default(),
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

/// The disabled rung leads but does NOT erase the health rungs beneath it: a
/// dead login is just as true on a disabled account, and hiding it would strand
/// an operator who re-enables it. Both facts stack on one `├│└` rail.
#[test]
fn status_lines_stacks_the_health_rungs_under_disabled() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut profile = crate::testutil::blank_profile("gamma");
    let header = HeaderState {
        is_active: false,
        account_email: None,
        diag: DiagFlags {
            auth_broken: true,
            ..DiagFlags::default()
        },
        ..disabled_rung_header(false)
    };

    // Control: enabled, the auth-broken rung is the whole block.
    let enabled = status_text(&status_lines(&profile, &header, 120));
    assert!(
        !enabled.contains("disabled"),
        "control: an enabled account shows no disabled pill: {enabled:?}"
    );

    profile.disabled = true;
    let lines = status_lines(&profile, &header, 120);
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
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Cached);
    let header = disabled_rung_header(true);

    // Control: enabled, both the kick pill and the fetch/countdown rungs render.
    let enabled = status_text(&status_lines(&profile, &header, 120));
    assert!(
        enabled.contains("cached"),
        "control: an enabled account reports its fetch state: {enabled:?}"
    );
    assert!(
        enabled.contains("blocked"),
        "control: the kick pill renders too: {enabled:?}"
    );

    profile.disabled = true;
    let rendered = status_text(&status_lines(&profile, &header, 120));
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
        ..crate::testutil::blank_profile("a")
    };
    assert_eq!(
        status_text(&status_lines(&clean, &disabled_rung_header(false), 120)),
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

    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Fresh);
    let header = |kick_block: Option<KickBlock>| HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block,
        diag: DiagFlags::default(),
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

    let clean = text(status_lines(&profile, &header(None), 120));
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

    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::RateLimited);
    let now = now_epoch_secs();
    // 52 inner cells is the narrowest pane the layout builds (an 80-col terminal
    // yields a 24-cell selector), so the block/fetch pills AND their rail hints
    // must all fit — the whole reason this row was split off a single ~83-cell one.
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 14_000),
            tick: 0,
            streaks: StreakCounts {
                rate_limit: 3,
                refresh_fail: 0,
            },
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 4 * 60 * 60),
                next_retry: now + 30,
            }),
            diag: DiagFlags::default(),
        },
        52,
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
/// (`├`/`│`/`└`, cloudy-tui Stacked hints) instead of floating as separate
/// detached `└` lines: a pill row sitting strictly between the first and last
/// hint bridges the rail at col 0 (`│` + blank padding to the value column),
/// every hint but the last branches off with `├`, and only the last closes
/// the rail with `└`.
#[test]
fn status_lines_connects_two_plus_hints_into_one_rail() {
    use crate::usage::KickBlock;

    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Cached);
    let now = now_epoch_secs();
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 45_000),
            tick: 0,
            streaks: StreakCounts {
                rate_limit: 0,
                refresh_fail: 3,
            },
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 3 * 60 * 60),
                next_retry: now + 30,
            }),
            diag: DiagFlags {
                auto_start: false,
                budget_spent: true,
                ..DiagFlags::default()
            },
        },
        120,
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
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Failed);
    let header = HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 20_000),
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: None,
        diag: DiagFlags {
            auth_broken: true,
            ..DiagFlags::default()
        },
    };
    let lines: Vec<String> = status_lines(&profile, &header, 120)
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
/// multi-line diagnostic reads as one unbroken stroke (cloudy-tui Stacked hints).
/// Guards `rail_hint_lines`' `cont = "│ "` branch — the width-120 rail test never
/// wraps into it, so a mutation to blank continuations otherwise stays green.
#[test]
fn status_lines_wrapped_non_last_hint_bridges_its_continuation() {
    use crate::usage::KickBlock;
    let now = now_epoch_secs();
    let mut profile = crate::testutil::blank_profile("a");
    // Kick (a long hint) + a cached fetch: two hints, so the kick hint is
    // non-last. At 30 cells the kick hint wraps; the fetch's shorter hint does not.
    profile.fetch_status = Some(FetchStatus::Cached);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 30_000),
            tick: 0,
            streaks: StreakCounts::default(),
            kick_block: Some(KickBlock {
                streak: 1,
                rejected: true,
                until: Some(now + 3 * 60 * 60),
                next_retry: now + 30,
            }),
            diag: DiagFlags {
                auto_start: true,
                ..DiagFlags::default()
            },
        },
        30,
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
/// pad, never a stray `│` below the closing `└` (cloudy-tui Stacked hints).
/// Guards `render_status_rows`' `seen < hint_count` upper bound on the bridge.
#[test]
fn status_lines_no_hint_row_after_closed_rail_stays_unbridged() {
    use crate::usage::KickBlock;
    let now = now_epoch_secs();
    let mut profile = crate::testutil::blank_profile("a");
    // Fetch FAILED carries no fix hint and renders last, so with kick +
    // budget-spent hints above it the rail closes on the spend `└` and the
    // failed row sits below the closed rail.
    profile.fetch_status = Some(FetchStatus::Failed);
    let lines: Vec<String> = status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 14_000),
            tick: 0,
            streaks: StreakCounts::default(),
            kick_block: Some(KickBlock {
                streak: 1,
                rejected: false,
                until: Some(now + 60 * 60),
                next_retry: now + 30,
            }),
            diag: DiagFlags {
                budget_spent: true,
                ..DiagFlags::default()
            },
        },
        120,
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

/// The `[ rate limited ]` suffix names which retry the countdown leads to
/// (`HeaderState.streak`) so a deep slot reads as stuck from the count alone;
/// a zero streak keeps the bare `retry in` suffix.
#[test]
fn rate_limited_suffix_counts_the_retry() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::RateLimited);
    let header = |streak: u32| HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks: StreakCounts {
            rate_limit: streak,
            refresh_fail: 0,
        },
        kick_block: None,
        diag: DiagFlags::default(),
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

    assert!(text(status_lines(&profile, &header(7), 120)).contains("7th retry in"));
    let bare = text(status_lines(&profile, &header(0), 120));
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
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Cached);
    let header = |refresh_fail: u32| HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks: StreakCounts {
            rate_limit: 0,
            refresh_fail,
        },
        kick_block: None,
        diag: DiagFlags::default(),
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
    let healthy = text(status_lines(&profile, &header(0), 120));
    assert!(healthy.contains("cached"), "got {healthy:?}");
    assert!(healthy.contains("refresh in"));
    assert!(
        !healthy.contains("auth failing"),
        "a cached row with a healthy chain must not cry auth"
    );

    // Failing: the pill names the cause and the countdown becomes a retry
    // ordinal, since it now leads to the next REFRESH attempt, not a poll.
    let failing = text(status_lines(&profile, &header(3), 120));
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
    let pill_style = |profile: &Profile, header: &HeaderState| {
        status_lines(profile, header, 120)
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("rate limited") || s.content.contains("auth failing"))
            .map(|s| s.style)
            .expect("a streak pill")
    };
    let header = |streaks: StreakCounts| HeaderState {
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streaks,
        kick_block: None,
        diag: DiagFlags::default(),
    };

    for (status, axis) in [
        (
            FetchStatus::RateLimited,
            (|n| StreakCounts {
                rate_limit: n,
                refresh_fail: 0,
            }) as fn(u32) -> StreakCounts,
        ),
        (FetchStatus::Cached, |n| StreakCounts {
            rate_limit: 0,
            refresh_fail: n,
        }),
    ] {
        let mut profile = crate::testutil::blank_profile("a");
        profile.fetch_status = Some(status);

        // At the cap the reading is still trusted — amber, same as `cached`.
        assert_eq!(
            pill_style(&profile, &header(axis(crate::usage::ACTIVE_CAP_MAX_STREAK))).fg,
            theme::warning().fg,
            "a streak at the cap is still a staleness cue, not a failure ({status:?})"
        );
        // One past it, the daemon distrusts the number; the row must agree.
        assert_eq!(
            pill_style(
                &profile,
                &header(axis(crate::usage::ACTIVE_CAP_MAX_STREAK + 1))
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
        is_active: false,
        account_email: None,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        streaks: StreakCounts::default(),
        kick_block: None,
        diag: DiagFlags::default(),
    };
    let with_window = |util: f64| {
        let mut p = crate::testutil::blank_profile("a");
        p.fetch_status = None;
        p.usage = Some(crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: util,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        });
        p
    };

    let spent = text(status_lines(&with_window(100.0), &header, 120));
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

    let below = text(status_lines(&with_window(50.0), &header, 120));
    assert!(
        below.contains("up to date"),
        "a below-cap idle account with no scheduled refresh is up to date: {below}"
    );
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
        let mut profile = crate::testutil::blank_profile("a");
        profile.usage = Some(crate::usage::UsageInfo {
            codex_rate_limit_reached: None,
            plan: None,
            five_hour: None,
            seven_day: None,
            weekly_scoped: Vec::new(),
            window_dollars: Vec::new(),
            extra_usage: extra,
            spend,
        });
        collect_stats(&profile, ResetFmt::default())
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
        diag_fix(UsageDiag::AuthBroken, "kerry"),
        "re-login with clauth login kerry"
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
        let mut profile = crate::testutil::blank_profile("a");
        profile.fetch_status = Some(FetchStatus::Fresh);
        joined(status_lines(
            &profile,
            &HeaderState {
                is_active: false,
                account_email: None,
                activity: ProfileActivity::Idle,
                next_refresh_ms: Some(now_ms() + 90_000),
                tick: 0,
                streaks: StreakCounts::default(),
                kick_block: Some(KickBlock {
                    streak: 2,
                    rejected: true,
                    until: Some(now + 4 * 60 * 60),
                    next_retry: now + 30,
                }),
                diag: DiagFlags {
                    auto_start,
                    ..DiagFlags::default()
                },
            },
            120,
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
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Fresh);
    let out = joined(status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            streaks: StreakCounts::default(),
            kick_block: None,
            diag: DiagFlags {
                spend_uncapped: true,
                budget_spent: true,
                ..DiagFlags::default()
            },
        },
        120,
    ));
    assert!(out.contains("[ uncapped ]") && out.contains("mark an account last resort"));
    assert!(
        !out.contains("[ extra usage spent ]"),
        "uncapped must suppress the extra-usage-spent pill: {out}"
    );
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
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::Cached);
    let out = joined(status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: Some(now_ms() + 90_000),
            tick: 0,
            streaks: StreakCounts {
                rate_limit: 0,
                refresh_fail: 3,
            },
            kick_block: Some(KickBlock {
                streak: 2,
                rejected: true,
                until: Some(now + 4 * 60 * 60),
                next_retry: now + 30,
            }),
            diag: DiagFlags {
                auth_broken: true,
                spend_uncapped: true,
                ..DiagFlags::default()
            },
        },
        120,
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
    let mut profile = crate::testutil::blank_profile("OmniRoute");
    profile.fetch_status = None;
    let out = joined(status_lines(
        &profile,
        &HeaderState {
            is_active: false,
            account_email: None,
            activity: ProfileActivity::Idle,
            next_refresh_ms: None,
            tick: 0,
            streaks: StreakCounts::default(),
            kick_block: None,
            diag: DiagFlags {
                auth_broken: true,
                ..DiagFlags::default()
            },
        },
        120,
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

// ── Fork-only tests (codex engine, RESCUE, CLA-FEED, forecast, email column) ──

#[test]
fn usage_header_names_the_linked_account() {
    let profile = crate::testutil::blank_profile("a");
    let header = |email: Option<&str>| HeaderState {
        streaks: StreakCounts::default(),
        kick_block: None,
        is_active: false,
        activity: ProfileActivity::Idle,
        next_refresh_ms: None,
        tick: 0,
        account_email: email.map(str::to_string),
        diag: DiagFlags::default(),
    };
    let text = |lines: &[Line<'static>]| -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    };

    let with = text(&header_lines(
        &profile,
        &header(Some("x@computelabs.ai")),
        60,
    ));
    let account = with
        .iter()
        .find(|l| l.starts_with("account"))
        .expect("account row renders when the email is cached");
    assert!(account.contains("x@computelabs.ai"), "{account}");
    let account_idx = with.iter().position(|l| l.starts_with("account")).unwrap();
    let status_idx = with.iter().position(|l| l.starts_with("status")).unwrap();
    assert!(
        account_idx < status_idx,
        "account sits between plan and status: {with:?}"
    );

    let without = text(&header_lines(&profile, &header(None), 60));
    assert!(
        !without.iter().any(|l| l.starts_with("account")),
        "no cached email → no account row: {without:?}"
    );
}
