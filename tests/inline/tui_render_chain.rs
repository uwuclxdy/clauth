//! `member_detail`'s all-exhausted "resumes: <name> in ~<eta>" caption on the
//! Fallback tab (issue #10 follow-up), driven by `crate::fallback::soonest_resume`.

use super::*;

fn snap_with_usage(mut s: ReadingsSnapshot, name: &str, u: UsageInfo) -> ReadingsSnapshot {
    s.usage.insert(name.to_string(), u);
    s
}
use crate::profile::{AppState, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

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

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Seed the app's stores from a `ReadingsSnapshot`, so a full-pane render
/// (which reads `app.readings_snapshot()`) shows the fixture's readings. The
/// chain fixtures are OAuth-only, so the picked `usage`/`fetch_status` maps
/// round-trip into the OAuth stores unchanged.
fn seed_stores(app: &App, readings: &ReadingsSnapshot) {
    app.usage_store
        .lock()
        .unwrap()
        .extend(readings.usage.clone());
    app.usage_status
        .lock()
        .unwrap()
        .extend(readings.fetch_status.clone());
}

fn app_with_readings(cfg: AppConfig, readings: &ReadingsSnapshot) -> App {
    let app = App::new(cfg);
    seed_stores(&app, readings);
    app
}

/// `QueueView` is the one resolver every chip goes through: membership from
/// the shared rule, the slot from the anchor. Pinned here because a view that
/// answered `None` for everyone would silently remove every chip on every
/// surface without failing a single render test.
#[test]
fn auto_start_queue_view_resolves_slots_from_config_and_anchor() {
    use crate::profile::{ClaudeCredentials, OAuthToken};
    let opted = |name: &str| {
        let (mut p, p_readings) = profile(name, 95.0, 10.0, 3600);
        p.auto_start = true;
        p.credentials = Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("{name}-access"),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..crate::profile::OAuthToken::default_extra()
            }),
        });
        (p, p_readings)
    };
    let (mut cfg, _readings) = config_with(
        vec![opted("a"), opted("b"), profile("c", 95.0, 0.0, 3600)],
        Some("a"),
        vec!["a", "b"],
    );
    cfg.state.auto_start_queue = true;

    let none = std::collections::HashMap::new();
    let view = super::super::panes::QueueView::new(&cfg, &none, None, |_| false);
    let slot = view.slot("a").expect("an opted-in member holds a slot");
    assert_eq!((slot.position, slot.total), (1, 2));
    assert_eq!(view.slot("b").map(|s| s.position), Some(2));
    assert_eq!(view.slot("c"), None, "no opt-in, no slot");
    assert_eq!(
        slot.next_in, None,
        "no anchor: the queue is due, nothing to count down"
    );

    // A switch-grade-blocked member holds no slot, and the queue re-sizes.
    let lifts = std::collections::HashMap::from([("a".to_string(), 0i64)]);
    let view = super::super::panes::QueueView::new(&cfg, &lifts, None, |_| false);
    assert_eq!(view.slot("a"), None, "blocked: no slot");
    assert_eq!(view.slot("b").map(|s| (s.position, s.total)), Some((1, 1)));

    // The toggle off is a real off switch on this surface too.
    cfg.state.auto_start_queue = false;
    let view = super::super::panes::QueueView::new(&cfg, &none, None, |_| false);
    assert_eq!(view.slot("a"), None);
}

// The auto-start queue line no longer renders on the Fallback card (owner
// 2026-09-01): it moved to the Usage tab, under `plan`, and shows for any
// account with `auto_start` on, queue toggle or no queue toggle.

// Whole chain exhausted: the caption renders under whichever member is
// selected, naming the soonest-resuming one (b resets sooner than a).
#[test]
fn all_exhausted_shows_resumes_hint_under_any_selected_member() {
    let (a, a_readings) = profile("a", 95.0, 100.0, 3600);
    let (b, b_readings) = profile("b", 95.0, 100.0, 1800);
    let (cfg, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );

    let on_a = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .2;
    let hint_a = on_a.expect("resumes hint renders while viewing member a");
    assert!(hint_a.contains("resumes: b in ~"), "{hint_a}");

    let on_b = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("b"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .2;
    let hint_b = on_b.expect("resumes hint renders while viewing member b");
    assert!(hint_b.contains("resumes: b in ~"), "{hint_b}");
}

// b still has headroom — chain isn't fully exhausted, caption stays hidden.
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let (a, a_readings) = profile("a", 95.0, 100.0, 3600);
    let (b, b_readings) = profile("b", 95.0, 20.0, 3600);
    let (cfg, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );

    let footnote = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .2;
    assert!(
        footnote.is_none(),
        "must not show when the chain isn't fully exhausted"
    );
}

/// The detail pane's content of a rendered row (after the `││` seam, trimmed
/// of the pane border and its 1-cell padding), empty on a blank interior row.
fn detail_of(row: &str) -> &str {
    row.split("││")
        .nth(1)
        .unwrap_or("")
        .trim_matches('│')
        .trim()
}

/// Render the Fallback body (not the full app frame) at `(w, h)`.
fn render_chain(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &app.readings_snapshot(), &[]))
        .unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// A two-member fully-exhausted chain: both at 100%, `a` resets later than `b`.
fn exhausted_cfg() -> (AppConfig, ReadingsSnapshot) {
    config_with(
        vec![
            profile("a", 95.0, 100.0, 3600),
            profile("b", 95.0, 100.0, 1800),
        ],
        Some("a"),
        vec!["a", "b"],
    )
}

// #A1: with the chain exhausted, `resumes:` pins on the detail pane's last
// interior row with a blank margin row above it, for EVERY member selected in
// turn (walking the selection frame-to-frame in one app, so the persisted
// scroll offset carries across the walk), at the 14-row short pane and the
// 24-row size the chain tests already use. The old code pushed the caption
// into the scroll content, where the focused scroll capped at the cursored row
// and never brought it into view.
#[test]
fn the_pinned_resumes_footnote_renders_at_every_height_for_every_member() {
    let _home = crate::testutil::HomeSandbox::new();
    for (w, h) in [(100u16, 14u16), (100u16, 24u16)] {
        let (cfg, readings) = exhausted_cfg();
        let mut app = app_with_readings(cfg, &readings);
        app.fallback_focus = FallbackFocus::Detail;
        for cursor in 0..2 {
            app.chain_cursor = cursor;
            let rows = render_chain(&app, w, h);
            let screen = rows.join("\n");
            let resumes = rows
                .iter()
                .position(|r| detail_of(r).starts_with("resumes:"))
                .unwrap_or_else(|| {
                    panic!("[{w}x{h} member {cursor}] resumes: never renders:\n{screen}")
                });
            assert_eq!(
                resumes,
                h as usize - 2,
                "[{w}x{h} member {cursor}] resumes: pins on the last interior row:\n{screen}"
            );
            // The margin row is blank by whole-row equality; the footnote row
            // carries the deterministic prefix (its `~` eta is clock-derived).
            assert_eq!(
                detail_of(&rows[resumes - 1]),
                "",
                "[{w}x{h} member {cursor}] a blank margin row sits above the footnote:\n{screen}"
            );
            assert!(
                detail_of(&rows[resumes]).starts_with("resumes: b in ~"),
                "[{w}x{h} member {cursor}] the footnote names the soonest-resuming member:\n{screen}"
            );
        }
    }
}

// #A2: the overflow scrollbar's track/thumb column never extends onto the
// margin row or the footnote row — the scrollbar spans the scroll area only.
#[test]
fn the_pinned_footnote_scrollbar_spans_only_the_scroll_area() {
    let _home = crate::testutil::HomeSandbox::new();
    let (w, h) = (100u16, 14u16);
    let (cfg, readings) = exhausted_cfg();
    let mut app = app_with_readings(cfg, &readings);
    app.fallback_focus = FallbackFocus::Detail;
    app.chain_cursor = 0;
    let rows = render_chain(&app, w, h);
    let screen = rows.join("\n");

    let resumes = rows
        .iter()
        .position(|r| detail_of(r).starts_with("resumes:"))
        .unwrap_or_else(|| panic!("resumes: never renders:\n{screen}"));
    // The scrollbar lives in the detail pane's right-padding column, the cell
    // just inside its right border.
    let track_col = w as usize - 2;
    let cell = |row_idx: usize| rows[row_idx].chars().nth(track_col).unwrap_or(' ');
    for off in 0..2 {
        let idx = resumes - 1 + off; // the margin row, then the footnote row
        assert!(
            !matches!(cell(idx), '┊' | '┃'),
            "row {idx} (margin/footnote) carries a scrollbar cell: {}",
            cell(idx)
        );
    }
    // The scroll content overflows at this height, so the scroll area itself
    // must still draw the track — the absence above is a span, not a missing bar.
    assert!(
        (0..resumes - 1).any(|idx| matches!(cell(idx), '┊' | '┃')),
        "no scrollbar cell on any scroll-area row:\n{screen}"
    );
}

// #A3: with headroom left, no footnote and no margin render — the scroll area
// takes the full height (absence assert).
#[test]
fn the_member_card_reserves_no_footnote_without_a_resume() {
    let _home = crate::testutil::HomeSandbox::new();
    let (cfg, readings) = config_with(
        vec![
            profile("a", 95.0, 100.0, 3600),
            profile("b", 95.0, 20.0, 3600),
        ],
        Some("a"),
        vec!["a", "b"],
    );
    for (w, h) in [(100u16, 14u16), (100u16, 24u16)] {
        let mut app = app_with_readings(cfg.clone(), &readings);
        app.fallback_focus = FallbackFocus::Detail;
        app.chain_cursor = 0;
        let rows = render_chain(&app, w, h);
        assert!(
            !rows.iter().any(|r| r.contains("resumes:")),
            "[{w}x{h}] a partially exhausted chain shows no footnote:\n{}",
            rows.join("\n")
        );
        // The scroll area takes the full height: at 14 rows the card overflows,
        // so the last interior row carries content (no reserved footnote/margin
        // rows sit empty at the bottom).
        if h == 14 {
            assert!(
                !detail_of(&rows[h as usize - 2]).is_empty(),
                "[{w}x{h}] the last interior row carries content (full-height scroll area):\n{}",
                rows.join("\n")
            );
        }
    }
}

// #A4: the pinned-footnote split rule, pinned pure: margin while the scroll
// area keeps 3 rows, margin dropped next, inlined past that.
#[test]
fn footnote_layout_pins_the_margin_drop_and_inline_rules() {
    use super::super::panes::{FootnoteLayout, footnote_layout};
    assert_eq!(
        footnote_layout(12, 1),
        FootnoteLayout::Pinned {
            scroll: 10,
            margin: 1
        },
        "roomy pane pins with the margin row"
    );
    assert_eq!(
        footnote_layout(5, 1),
        FootnoteLayout::Pinned {
            scroll: 3,
            margin: 1
        },
        "exactly 3 scroll rows keeps the margin"
    );
    assert_eq!(
        footnote_layout(4, 1),
        FootnoteLayout::Pinned {
            scroll: 3,
            margin: 0
        },
        "a scroll area under 3 with the margin drops the margin first"
    );
    assert_eq!(
        footnote_layout(3, 1),
        FootnoteLayout::Inline,
        "smaller still inlines the footnote into the scroll content"
    );
    assert_eq!(
        footnote_layout(12, 0),
        FootnoteLayout::Absent,
        "no footnote, full height"
    );
}

// #A4 render: at a pane whose scroll area is exactly 3 rows the margin row
// drops (the footnote pins directly below the scroll content), and at a pane
// too short to pin, walking to the last block scrolls the inline footnote into
// view instead of stranding it.
#[test]
fn the_short_pane_footnote_drops_the_margin_then_inlines_reachably() {
    let _home = crate::testutil::HomeSandbox::new();

    // body 6 → detail interior 4 → Pinned { scroll: 3, margin: 0 }: the
    // footnote pins on the last interior row with NO blank row above it.
    let (cfg, readings) = exhausted_cfg();
    let mut app = app_with_readings(cfg, &readings);
    app.fallback_focus = FallbackFocus::Detail;
    app.chain_cursor = 0;
    let rows = render_chain(&app, 100, 6);
    let screen = rows.join("\n");
    let resumes = rows
        .iter()
        .position(|r| detail_of(r).starts_with("resumes:"))
        .unwrap_or_else(|| panic!("resumes: never renders:\n{screen}"));
    assert_eq!(
        resumes, 4,
        "the footnote pins on the last interior row:\n{screen}"
    );
    assert!(
        !detail_of(&rows[resumes - 1]).is_empty(),
        "no margin row above it — the row above is scroll content:\n{screen}"
    );

    // body 5 → detail interior 3 → Inline: walking to the last row reaches it.
    let (cfg, readings) = exhausted_cfg();
    let mut app = app_with_readings(cfg, &readings);
    app.fallback_focus = FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = FALLBACK_ROWS.len() - 1; // the last row
    let rows = render_chain(&app, 100, 5);
    assert!(
        rows.iter().any(|r| detail_of(r).starts_with("resumes:")),
        "walking to the last block scrolls the inline footnote into view:\n{}",
        rows.join("\n")
    );
}

// ── help-hint wrapping + dynamic copy ────────────────────────────────────────

// A narrow detail pane wraps the selected row's hint into `└ `-led +
// indented continuation lines instead of clipping it off the pane edge.
#[test]
fn last_resort_hint_wraps_on_a_narrow_pane() {
    let (a, a_readings) = profile("a", 95.0, 20.0, 3600);
    let (b, b_readings) = profile("b", 95.0, 20.0, 3600);
    let (cfg, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );

    // Focused on the `last resort` row at 28 cols.
    let lr = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::LastResort)
        .unwrap();
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: lr,
            width: 28,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let lead = texts
        .iter()
        .position(|t| t.starts_with(" └ "))
        .expect("hint leader line renders");
    assert!(
        texts[lead].chars().count() <= 28,
        "first hint line must fit the pane: {:?}",
        texts[lead]
    );
    // Exactly the leader's width, so the continuation stacks under the text
    // rather than under the `└` (or one cell past it).
    assert!(
        texts[lead + 1].starts_with("   ") && !texts[lead + 1].starts_with("    "),
        "hint continues on an indented line instead of clipping: {:?}",
        texts[lead + 1]
    );
}

// The last-resort hint names the member the exclusive mark would move from.
#[test]
fn last_resort_hint_names_the_currently_marked_member() {
    let (a, a_readings) = profile("a", 95.0, 20.0, 3600);
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.last_resort = true;
    let (cfg, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: FALLBACK_ROWS
                .iter()
                .position(|r| *r == FallbackRow::LastResort)
                .unwrap(),
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let hint = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("└"))
        .expect("hint renders");
    assert!(hint.contains("instead of 'b'"), "{hint}");
}

// A day list outranks the toggle, so the hint has to name the days rather than
// the toggle's state: read the stock wording with a list set and an operator
// would take the toggle for the answer.
#[test]
fn preferred_hint_names_the_day_list_when_one_is_set() {
    let (mut a, a_readings) = profile("a", 95.0, 20.0, 3600);
    a.preferred = false;
    a.preferred_days = vec![chrono::Weekday::Sat, chrono::Weekday::Sun];
    let (cfg, readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Preferred)
        .unwrap();

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: row,
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    assert_eq!(
        hint_after(&lines, "preferred").as_deref(),
        Some("home on sat, sun by its preferred days"),
        "the list lives on the card now, not in a file"
    );
}

// A list this account could not serve claims nothing, so the card must not
// promise home on those days — `is_home_on` refuses the same claim. The
// eligibility check is the one the `claimed_elsewhere` branch already applied
// to the other side; the reason lives on the card's own `preferred days` row.
#[test]
fn preferred_hint_drops_a_day_list_the_account_cannot_serve() {
    let (mut a, a_readings) = profile("a", 95.0, 20.0, 3600);
    a.preferred = true;
    a.preferred_days = vec![chrono::Weekday::Sat];
    a.disabled = true;
    let (cfg, readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Preferred)
        .unwrap();

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: row,
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let hint = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("\u{2514}"))
        .expect("hint renders");
    assert!(
        !hint.contains("home on sat"),
        "an inert list must not read as a home day: {hint}"
    );
}

// With a list elsewhere the toggle still answers, but only for the days that
// list leaves alone — the branch the stock wording ("once it's free again")
// would read as every day.
#[test]
fn preferred_hint_says_which_days_are_left_when_another_account_claims() {
    let (mut a, a_readings) = profile("a", 95.0, 20.0, 3600);
    a.preferred = true;
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.preferred_days = vec![chrono::Weekday::Sat, chrono::Weekday::Sun];
    let (cfg, readings) = config_with(
        vec![(a, a_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Preferred)
        .unwrap();

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: row,
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let hint = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("└"))
        .expect("hint renders");
    assert!(hint.contains("no day list claims"), "{hint}");
}

// A list AND the flag on one account is home on the listed days by the list
// and on the days no list claims by the flag, so naming only the list would
// read as standing down for the other five.
#[test]
fn preferred_hint_adds_the_unclaimed_days_when_the_flag_is_also_on() {
    let (mut a, a_readings) = profile("a", 95.0, 20.0, 3600);
    a.preferred = true;
    a.preferred_days = vec![chrono::Weekday::Sat, chrono::Weekday::Sun];
    let (cfg, readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Preferred)
        .unwrap();

    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: row,
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    // "The rest" would be false the moment another member's list names one of
    // those days, so the toggle's half names the days it really decides.
    assert_eq!(
        hint_after(&lines, "preferred").as_deref(),
        Some("home on sat, sun by its preferred days, and on days no list claims by this toggle")
    );
}

// The per-account usage-gate rows render as toggles whose hint states the
// CURRENT walk behavior for this account, flipping wording with the gate.
#[test]
fn usage_gate_rows_hint_the_current_state() {
    let texts = |(p, p_readings): (Profile, ReadingsSnapshot), cursor: usize| -> Vec<String> {
        let (cfg, readings) = config_with(vec![(p, p_readings)], Some("a"), vec!["a"]);
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: cursor,
                width: 80,
                ..Default::default()
            },
            &HashSet::new(),
            &readings,
        )
        .0
        .iter()
        .map(line_text)
        .collect()
    };

    // FALLBACK_ROWS[2] == CheckWeekly.
    let on = texts(profile("a", 95.0, 20.0, 3600), 2);
    assert!(on.iter().any(|t| t.contains("weekly gate")), "{on:?}");
    assert!(
        on.iter()
            .any(|t| t.contains("out of rotation") && t.contains("weekly usage")),
        "{on:?}"
    );
    let (mut p, p_readings) = profile("a", 95.0, 20.0, 3600);
    p.check_weekly = false;
    let off = texts((p, p_readings), 2);
    assert!(off.iter().any(|t| t.contains("isn't checked")), "{off:?}");

    // FALLBACK_ROWS[3] == CheckScoped.
    let on = texts(profile("a", 95.0, 20.0, 3600), 3);
    assert!(on.iter().any(|t| t.contains("scoped gate")), "{on:?}");
    assert!(on.iter().any(|t| t.contains("per-model week")), "{on:?}");
    let (mut p, p_readings) = profile("a", 95.0, 20.0, 3600);
    p.check_scoped = false;
    let off = texts((p, p_readings), 3);
    assert!(
        off.iter()
            .any(|t| t.contains("stays in rotation for other models")),
        "{off:?}"
    );
}

// A gated-on per-model week over the line pills the detail card with its
// label; the gate off drops it (chip and walk must not drift).
#[test]
fn scoped_spent_pill_names_the_window_and_respects_the_gate() {
    let scoped = |check_scoped: bool| -> (Profile, ReadingsSnapshot) {
        let (mut p, mut p_readings) = profile("a", 95.0, 20.0, 3600);
        p.check_scoped = check_scoped;
        if let Some(u) = p_readings.usage.get_mut("a") {
            u.seven_day = Some(UsageWindow {
                utilization: 40.0,
                resets_at: Some(reset_in(5 * 86_400)),
            });
            u.weekly_scoped = vec![crate::usage::ScopedWindow {
                label: "7d fable".to_string(),
                window: UsageWindow {
                    utilization: 100.0,
                    resets_at: Some(reset_in(5 * 86_400)),
                },
            }];
        }
        (p, p_readings)
    };

    let (cfg, readings) = config_with(vec![scoped(true)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let pill = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("7d fable"))
        .expect("scoped pill renders");
    assert!(pill.contains("other models ok"), "{pill}");

    let (cfg, readings) = config_with(vec![scoped(false)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    assert!(
        !lines.iter().map(line_text).any(|t| t.contains("7d fable")),
        "gate off must drop the scoped pill"
    );
}

// The `max spend` hint names whichever half of the opt-in is holding spending
// back, and shows the REAL armed room when both are set. Five distinct copies,
// one per spend state — `spend_room` fails closed on money, so an unknown spend
// never reads as a $0 figure.
#[test]
fn max_spend_hint_covers_every_spend_state() {
    use crate::usage::SpendInfo;
    let hint = |budget_on: bool, ceiling: Option<f64>, spend: Option<SpendInfo>| -> String {
        let (mut a, mut a_readings) = profile("a", 95.0, 40.0, 7200);
        a.max_auto_spend = ceiling;
        a_readings.usage.get_mut("a").unwrap().spend = spend;
        let (mut cfg, readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
        cfg.state.spend_budget_switching = budget_on;
        max_spend_hint(
            &cfg,
            cfg.profiles[0].max_auto_spend.unwrap_or(0.0),
            readings.usage.get("a"),
        )
    };
    let billing = |enabled: bool, used: Option<f64>| SpendInfo {
        enabled,
        used,
        limit: Some(20.0),
        percent: None,
        currency: None,
    };

    // 1. chain toggle off
    assert!(
        hint(false, Some(10.0), Some(billing(true, Some(1.0))))
            .contains("turn on allow extra usage")
    );
    // 2. no ceiling
    assert!(hint(true, None, Some(billing(true, Some(1.0)))).contains("type a ceiling"));
    // 3. account not billing
    assert!(
        hint(true, Some(10.0), Some(billing(false, Some(1.0))))
            .contains("isn't set up for paid usage")
    );
    // 4. spend unknown → the ceiling statement, never an invented $0 room
    let unknown = hint(true, Some(10.0), Some(billing(true, None)));
    assert!(unknown.contains("spends at most $10.00"), "{unknown}");
    // 5. armed → the real room: 0.9 * min(20, 10) - 1 = $8.00
    let armed = hint(true, Some(10.0), Some(billing(true, Some(1.0))));
    assert!(armed.contains("$8.00 left to spend"), "{armed}");
}

// ── key-column alignment ────────────────────────────────────────────────────

/// Column a row's value opens at: the first non-space cell past the key text.
/// `str::find` is byte-based, so re-count chars for the head to stay glyph-
/// accurate past any multi-byte arrow (e.g. `❯`).
fn value_col(key: &str, rendered: &str) -> usize {
    let after = rendered.find(key).expect("row renders its key") + key.len();
    let head_chars = rendered[..after].chars().count();
    let gap = rendered[after..].chars().take_while(|c| *c == ' ').count();
    head_chars + gap
}

// `last resort` is exactly the old `KEY_W` (11) chars, so a
// `saturating_sub(len).max(1)` pad pushed its value a column right of every
// other interactive row. The shared `key_cell` keeps the gap separate from the
// width, so every interactive row opens its value at the same column.
#[test]
fn last_resort_value_aligns_with_other_rows() {
    let (a, a_readings) = profile("a", 95.0, 20.0, 3600);
    let (cfg, readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let texts: Vec<String> = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 1,
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0
    .iter()
    .map(line_text)
    .collect();

    let rotate = texts
        .iter()
        .find(|t| t.contains("rotate at"))
        .expect("rotate at row");
    let last = texts
        .iter()
        .find(|t| t.contains("last resort"))
        .expect("last resort row");
    let remove = texts
        .iter()
        .find(|t| t.contains("remove"))
        .expect("remove row");

    let col = value_col("rotate at", rotate);
    assert_eq!(
        col,
        value_col("last resort", last),
        "`last resort` (== old KEY_W chars) must not push its value column right"
    );
    assert_eq!(
        col,
        value_col("remove", remove),
        "all rows share the value column"
    );

    let spend = texts
        .iter()
        .find(|t| t.contains("max spend"))
        .expect("max spend row");
    assert_eq!(
        col,
        value_col("max spend", spend),
        "all rows share the value column"
    );
}

/// The ceiling row reads as a state, not a bare number: $0 is the never-spend
/// default and must not look like a figure the operator dialled in. A set
/// ceiling renders as money, with the cents, since it is money.
#[test]
fn max_spend_row_renders_off_at_zero_and_dollars_when_set() {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    let row = |c: &crate::profile::AppConfig| -> String {
        member_detail(
            c,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: 1,
                width: 60,
                ..Default::default()
            },
            &HashSet::new(),
            &readings,
        )
        .0
        .iter()
        .map(line_text)
        .find(|t| t.contains("max spend"))
        .expect("max spend row")
    };
    assert!(
        row(&cfg).contains("off"),
        "unset reads as off: {:?}",
        row(&cfg)
    );
    assert!(!row(&cfg).contains('$'), "no dollar figure when off");

    let (mut armed, _readings) =
        config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    armed.profiles[0].max_auto_spend = Some(25.0);
    assert!(
        row(&armed).contains("$25.00"),
        "a set ceiling renders as money: {:?}",
        row(&armed)
    );
}

// ── disabled chain member (feature: per-account disable toggle) ─────────────

/// `Disabled` and `Canceled` share the `⊖` shape and split on hue alone — the
/// one deliberate departure from the shape-names-the-state rule (see
/// `reason_marker`). Pinned here because giving either arm its own shape puts
/// the same account under two glyphs across the Overview's two panels: the
/// account row picks the canceled arm where this ladder picks the disabled one.
#[test]
fn disabled_and_canceled_share_the_marker_shape_and_split_on_hue() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dis = reason_marker(&BlockedReason::Disabled);
    let can = reason_marker(&BlockedReason::Canceled);
    assert_eq!(dis.content, "⊖", "disabled marker shape");
    assert_eq!(can.content, "⊖", "canceled marker shape");
    assert_eq!(dis.style.fg, theme::faint().fg, "disabled reads uncharged");
    assert_eq!(can.style.fg, theme::danger().fg, "canceled reads dead");
}

/// Stale and no-usage are two hazards, so they take two shapes (#116 review):
/// a stale reading is a measurement that went old and the walk will leave such
/// an active once it is deep-stuck; an unmeasured member is never read at all
/// and the walk never leaves it.
///
/// The shape is SOLID (`◆`): a run in a real terminal showed the dotted `◌`
/// reading as a light grey ring, because a hairline glyph carries almost none
/// of this palette's pastel amber, and `◌` is the Usage tab's queued-refresh
/// dot besides. The pill carries the warning hue too, so the card is charged
/// even where the 1-cell mark is off screen.
#[test]
fn stale_and_no_usage_split_on_shape_and_on_hue() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let stale = reason_marker(&BlockedReason::Stale);
    let none = reason_marker(&BlockedReason::NoUsage);
    assert_eq!(none.content, "◆", "its own shape, and a solid one");
    assert_ne!(none.content, stale.content, "not stale's shape");
    assert_eq!(none.style.fg, theme::warning().fg, "warning, not faint");
    assert_ne!(none.style.fg, stale.style.fg, "not stale's hue either");
    // The 5h glyph is the one other round shape on the card; it cannot co-occur
    // with this rung (it needs a reading past the line, this rung fires only
    // when no reading is acting), but it must still read as a different mark.
    assert_ne!(
        none.content,
        reason_marker(&BlockedReason::FiveHour {
            pct: 99.0,
            resets_in: None
        })
        .content,
        "distinct from the 5h mark"
    );
    let pill = |r: &BlockedReason| -> String {
        reason_pill_spans(r, ResetFmt::default())
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    // The word says what clauth CANNOT DO. `no usage yet` read as "the first
    // poll is still coming", which is this state's opposite, and it is what
    // `start --auto --explain` prints for an account whose poll really is
    // pending. The verb is the add confirm's and the daemon line's.
    assert_eq!(pill(&BlockedReason::NoUsage), "[ can't read usage ]");
    assert_ne!(pill(&BlockedReason::NoUsage), pill(&BlockedReason::Stale));
    // The label itself is charged, not just the 1-cell mark in the selector:
    // the card showed this pill in the neutral grey of its own `status` label,
    // one row under a red `[ auth broken ]`.
    let label = |r: &BlockedReason| {
        reason_pill_spans(r, ResetFmt::default())
            .into_iter()
            .find(|s| s.content.contains("read usage") || s.content.contains("stale"))
            .expect("the label span")
            .style
    };
    assert_eq!(
        label(&BlockedReason::NoUsage).fg,
        theme::warning().fg,
        "the pill label carries the warning hue"
    );
    assert_ne!(
        label(&BlockedReason::NoUsage).fg,
        label(&BlockedReason::Stale).fg,
        "and stale stays dim"
    );
}

/// \#110: a member clauth cannot measure opens its card on the `can't read
/// usage` pill, and the line under it names the hazard and then the fix. Both
/// pinned by EQUALITY, as `key_rejected_copy_is_pinned_by_equality` does: a
/// `contains` let either end drift and stay green (#116 review). The card is
/// rendered wide enough that the hint is one line, so the pin reads as the
/// operator sees it.
///
/// The hazard is pinned in BOTH tenses. A run in a real terminal met the card
/// of a member the chain had already walked onto, and it still said "once it's
/// active": the one state the warning exists for was the one state it spoke
/// about in the conditional.
#[test]
fn a_member_clauth_cannot_measure_opens_its_card_on_the_no_usage_pill() {
    // The fixture reading is DROPPED rather than blanked: a member nothing
    // polls has no entry in the snapshot at all. The sidecar half arrives on
    // the card itself, the way the render path hands it over from
    // `App::session_tokens`.
    let card = |session_token: bool, active: &str| {
        let (blind, _unpolled) = profile("blind", 95.0, 0.0, 7200);
        let (cfg, readings) = config_with(
            vec![
                profile("ok", 95.0, 10.0, 7200),
                (blind, ReadingsSnapshot::default()),
            ],
            Some(active),
            vec!["ok", "blind"],
        );
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("blind"),
            MemberCard {
                width: 160,
                session_token,
                ..Default::default()
            },
            &HashSet::new(),
            &readings,
        )
        .0
    };
    let lines = card(true, "ok");
    assert_eq!(
        line_text(&lines[0]),
        "status          [ can't read usage ]",
        "the pill row, with nothing after the pill"
    );
    assert_eq!(
        line_text(&lines[1]),
        "└ once it's active, clauth won't switch away from it on its own: log in the same \
         account with clauth login blind so clauth can read its usage",
        "the hazard in the future tense while another member is active"
    );
    // The gauge row keeps upstream's own word: with the pill saying what clauth
    // cannot do, this is no longer a second phrasing of one fact.
    let gauge = lines
        .iter()
        .map(line_text)
        .find(|t| t.starts_with("5h usage"))
        .expect("the 5h row");
    assert!(
        gauge.ends_with("no data yet"),
        "the gauge row says there is no figure: {gauge:?}"
    );

    // The same member, now the active one: the hazard is the state of the
    // machine, not a forecast.
    assert_eq!(
        line_text(&card(true, "blind")[1]),
        "└ it's active, and clauth won't switch away from it on its own: log in the same \
         account with clauth login blind so clauth can read its usage",
        "the hazard in the present tense once the chain has landed here"
    );

    // The same fixture without the sidecar: nothing serves from it, so this is
    // the logged-out shape and the card opens on its gauge instead of the pill.
    assert_ne!(
        line_text(&card(false, "ok")[0]),
        line_text(&lines[0]),
        "without a session token the card carries no no-usage pill"
    );
}

/// \#116 review: a member the operator disabled is not a member clauth cannot
/// measure. The card stacks the health ladder under `[ disabled ]`
/// (`member_detail_stacks_the_health_pill_under_disabled`), so while the
/// marker's gate answered disabled-ness too, every disabled member holding a
/// login and a long-lived token read `can't read usage` there and was told to
/// log in the account it is already logged into. Normal operation reaches that
/// shape: a setup token beside a login is what the fix line itself leads to,
/// and a rolling-token account disabled on the Setup tab is the same shape.
///
/// So the pin is the whole opening block, not the absence of one pill: the
/// exclusion, its own fix, and the blank the pill block ends on.
#[test]
fn a_disabled_member_with_a_login_is_not_called_unmeasurable() {
    use crate::profile::{ClaudeCredentials, OAuthToken};
    let (mut d, d_readings) = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    d.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "a-access".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    });
    let (cfg, readings) = config_with(vec![(d, d_readings)], Some("other"), vec!["a"]);
    let (lines, _, _) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 160,
            session_token: true,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    );
    assert_eq!(
        lines.iter().take(3).map(line_text).collect::<Vec<_>>(),
        vec![
            "status          [ disabled ]".to_string(),
            "└ excluded from the walk, enable it on the setup tab".to_string(),
            String::new(),
        ],
        "the exclusion is the whole status block: one pill, one fix, no second rung"
    );
}

/// A disabled chain member — still configured in `fallback_chain` on disk,
/// only the walk skips it (see `Profile::is_disabled`)
/// — dims its name in the Fallback selector and carries the `⊖` blocked-reason
/// marker, with the `[ disabled ]` label reaching the operator through the
/// detail card's `reason_pill`. The add-picker exclusion (a disabled account
/// can never be (re-)added) is a pure-logic concern covered separately in
/// `tests/inline/tui_app.rs`'s `chain_candidates_excludes_a_disabled_profile`.
#[test]
fn disabled_chain_member_dims_its_name_and_takes_the_blocked_reason_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut a, a_readings) = profile("xqzacct", 95.0, 10.0, 3600);
    a.disabled = true;
    let (cfg, _readings) = config_with(vec![(a, a_readings)], None, vec!["xqzacct"]);
    let app = App::new(cfg);

    let (w, h) = (100u16, 14u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);

    // The detail pane's own title border also carries the bare name
    // (`section_box_verbatim`), so require the marker alongside it to land on
    // the selector's list row specifically.
    let row_idx = rows
        .iter()
        .position(|r| r.contains("xqzacct") && r.contains('⊖'))
        .unwrap_or_else(|| {
            panic!(
                "no row carries both the member name and the ⊖ marker:\n{}",
                rows.join("\n")
            )
        });
    let row = &rows[row_idx];
    // Buffer COLUMN, not `str::find`'s byte offset — the pane border and the
    // marker are multi-byte, so the two diverge on exactly this row.
    let col_of = |needle: &str| -> usize {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} renders"));
        row[..byte].chars().count()
    };
    let cell = &buf.content[row_idx * w as usize + col_of("xqzacct")];
    assert_eq!(
        Some(cell.fg),
        theme::dim().fg,
        "the disabled member's name cell renders dim, not name_color's active/inactive branch"
    );

    let marker_cell = &buf.content[row_idx * w as usize + col_of("⊖")];
    assert_eq!(
        Some(marker_cell.fg),
        theme::faint().fg,
        "the ⊖ marker is uncharged, matching ⋯ stale"
    );
    // ⊖ is shared with `Canceled` (see `reason_marker`), so the hue is the only
    // thing telling the two apart on this row.
    assert_ne!(
        Some(marker_cell.fg),
        theme::danger().fg,
        "a disabled member must not wear the canceled arm's danger hue"
    );

    // Both panes share this physical row, so split at the seam: the selector
    // half carries the marker alone, the label lives on the detail card's pill.
    let seam = row.find("││").expect("the two panes meet on this row");
    let (selector, detail) = row.split_at(seam);
    assert!(
        !selector.contains("disabled"),
        "the selector row carries the marker only, no inline chip: {selector}"
    );
    assert!(
        detail.contains("[ disabled ]"),
        "the detail card shows the `[ disabled ]` pill: {detail}"
    );
}

/// `BlockedReason::Disabled` outranks every other reason: a disabled account is
/// skipped as a candidate regardless of what its usage or credentials say, so
/// naming a quota/liveness block instead would describe a member nothing picks.
#[test]
fn blocked_reason_ranks_disabled_above_canceled_and_auth_broken() {
    use crate::fallback::{BlockedReason, blocked_reason};
    use crate::usage::PlanInfo;

    // Canceled subscription AND a broken login AND a maxed 5h window at once.
    let (mut a, mut a_readings) = profile("acct", 50.0, 100.0, 3600);
    a.disabled = true;
    a_readings.usage.get_mut("acct").unwrap().plan = Some(PlanInfo {
        subscription_status: Some("canceled".to_string()),
        ..PlanInfo::default()
    });
    let (mut cfg, readings) = config_with(vec![(a, a_readings)], Some("other"), vec!["acct"]);
    cfg.state.auth_broken.push("acct".into());

    let p = cfg
        .find(&crate::profile::ProfileName::from("acct"))
        .unwrap();
    assert_eq!(
        blocked_reason(
            &cfg,
            p,
            None,
            &HashSet::new(),
            readings.usage.get(p.name.as_str()),
            readings.status_of(p.name.as_str()),
            false
        ),
        Some(BlockedReason::Disabled),
        "disabled ranks first, above canceled and auth broken"
    );

    // Flipping only the disabled bit hands the row back to the next rung.
    let mut enabled = cfg.clone();
    enabled.profiles[0].disabled = false;
    assert_eq!(
        blocked_reason(
            &enabled,
            enabled
                .find(&crate::profile::ProfileName::from("acct"))
                .unwrap(),
            None,
            &HashSet::new(),
            readings.usage.get(
                enabled
                    .find(&crate::profile::ProfileName::from("acct"))
                    .unwrap()
                    .name
                    .as_str()
            ),
            readings.status_of(
                enabled
                    .find(&crate::profile::ProfileName::from("acct"))
                    .unwrap()
                    .name
                    .as_str()
            ),
            false
        ),
        Some(BlockedReason::Canceled),
        "without the disabled bit the canceled rung wins"
    );
}

/// The non-active guard: `snapshot_chain` and the walk deliberately never
/// drop a disabled ACTIVE member from the walk (the bit is candidate-only), so
/// claiming `Disabled` there would be the second opinion `blocked_reason` must
/// never be. Unreachable through `actions::disable_profile` today — which
/// refuses an active target — but the walk already guards it, so this does too.
#[test]
fn blocked_reason_never_reports_disabled_for_the_active_profile() {
    use crate::fallback::{BlockedReason, blocked_reason};

    let (mut a, a_readings) = profile("acct", 95.0, 10.0, 3600);
    a.disabled = true;
    let (cfg, readings) = config_with(vec![(a, a_readings)], Some("acct"), vec!["acct"]);
    assert_eq!(
        blocked_reason(
            &cfg,
            cfg.find(&crate::profile::ProfileName::from("acct"))
                .unwrap(),
            None,
            &HashSet::new(),
            readings.usage.get(
                cfg.find(&crate::profile::ProfileName::from("acct"))
                    .unwrap()
                    .name
                    .as_str()
            ),
            readings.status_of(
                cfg.find(&crate::profile::ProfileName::from("acct"))
                    .unwrap()
                    .name
                    .as_str()
            ),
            false
        ),
        None,
        "a disabled ACTIVE member has headroom and reports no block"
    );

    // The same profile, no longer active, does report it.
    let mut inactive = cfg.clone();
    inactive.state.active_profile = Some("other".into());
    assert_eq!(
        blocked_reason(
            &inactive,
            inactive
                .find(&crate::profile::ProfileName::from("acct"))
                .unwrap(),
            None,
            &HashSet::new(),
            readings.usage.get(
                inactive
                    .find(&crate::profile::ProfileName::from("acct"))
                    .unwrap()
                    .name
                    .as_str()
            ),
            readings.status_of(
                inactive
                    .find(&crate::profile::ProfileName::from("acct"))
                    .unwrap()
                    .name
                    .as_str()
            ),
            false
        ),
        Some(BlockedReason::Disabled),
        "a disabled NON-active member reports the block"
    );
}

// ── blocked-reason pill (detail card, weekly-fallback §4) ────────────────────

// A member over its 5h threshold shows the worst-reason pill at the very top of
// the card, naming the block with its utilization % and reset countdown.
#[test]
fn blocked_member_shows_the_worst_reason_pill() {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 97.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let pill = line_text(&lines[0]);
    assert!(pill.contains('['), "renders as a status pill: {pill:?}");
    assert!(
        pill.contains("5h 97%"),
        "names the 5h block with %: {pill:?}"
    );
    // Tolerant on the exact bucket: the fixture's `now` and `blocked_reason`'s
    // `now` can straddle a whole second (7200 → "1h 59m"), so assert only that a
    // countdown suffix trails the pill, not its value.
    assert!(
        pill.contains("[ 5h 97% ]  "),
        "carries the reset countdown as a trailing suffix: {pill:?}"
    );
    assert!(!pill.contains('·'), "no middle-dot separator: {pill:?}");
}

// A switch-grade kick-rejected member — headroom, but the messages limiter won't
// let clauth start it — shows the `claude code blocked` pill driven by `kick_lift`.
#[test]
fn kick_rejected_member_shows_the_claude_code_blocked_pill() {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let until = now_epoch_secs() + 7200;
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            kick_lift: Some(until),
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let pill = line_text(&lines[0]);
    // Bare pill + a faint countdown suffix OUTSIDE the brackets (no `·`), the
    // same shape the Usage-tab kick pill renders. The exact bucket stays tolerant
    // since the two `now` reads (fixture vs `blocked_reason`) can straddle a whole
    // second. The exact `lifts_in` value is range-checked in the
    // `blocked_reason_kick_*` unit test instead.
    assert!(
        pill.contains("[ claude code blocked ]  "),
        "renders the kick pill with a trailing lift countdown: {pill:?}"
    );
    assert!(!pill.contains('·'), "no middle-dot separator: {pill:?}");
}

// The canceled pill reads the short shared label, not the old verbose
// `subscription canceled`: the `└` hint carries the explanation, and the label
// comes from the one source both this card and the Usage status block read, so
// the two tabs can't drift apart again.
#[test]
fn canceled_member_shows_the_short_shared_label() {
    let rendered: String = reason_pill_spans(&BlockedReason::Canceled, ResetFmt::default())
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(rendered, "[ canceled ]", "got {rendered:?}");
    assert!(
        !rendered.contains("subscription"),
        "the verbose wording moved to the hint line: {rendered:?}"
    );
}

/// P1: a reason pill quoting a usage reading prints it with at most two
/// nearest-rounded decimals, like the bar it summarizes.
#[test]
fn reason_pills_keep_two_rounded_decimals() {
    let pill = |reason: BlockedReason| -> String {
        reason_pill_spans(&reason, ResetFmt::default())
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert_eq!(
        pill(BlockedReason::FiveHour {
            pct: 73.42,
            resets_in: None
        }),
        "[ 5h 73.42% ]"
    );
    assert_eq!(
        pill(BlockedReason::WeeklySoft {
            pct: 99.999,
            monthly: false
        }),
        "[ weekly 100% ]  still serving"
    );
    assert_eq!(
        pill(BlockedReason::ScopedSpent {
            label: "7d fable".to_string(),
            pct: 1.000_000_000_000_000_9
        }),
        "[ 7d fable 1% ]  other models ok"
    );
}

/// The soft-block pill names a monthly window (`30d 99%`), never "weekly" —
/// the owner's copy ruling, pinned on the render arm itself.
#[test]
fn the_soft_block_pill_names_a_monthly_window() {
    let pill: String = reason_pill_spans(
        &BlockedReason::WeeklySoft {
            pct: 99.0,
            monthly: true,
        },
        ResetFmt::default(),
    )
    .iter()
    .map(|s| s.content.as_ref())
    .collect();
    assert_eq!(pill, "[ 30d 99% ]  still serving");
}

/// The spent-block fix names a monthly quota (`monthly quota is spent`), never
/// "weekly limit is spent" — the owner's copy ruling, pinned on the render arm
/// itself.
#[test]
fn the_spent_fix_names_a_monthly_quota() {
    assert_eq!(
        reason_fix(
            &BlockedReason::WeeklySpent {
                resets_in: None,
                monthly: true,
            },
            &crate::profile::ProfileName::from("acct"),
            false,
        ),
        "monthly quota is spent"
    );
}

/// The spent pill names a monthly quota (`30d spent`), never "weekly spent" —
/// the owner's copy ruling, pinned on the render arm itself. (`30d spent`
/// rides two arms: this one and `format.rs`'s `start_block_label`; both are
/// pinned separately so neither can revert to the weekly word alone.)
#[test]
fn the_spent_pill_names_a_monthly_quota() {
    let pill: String = reason_pill_spans(
        &BlockedReason::WeeklySpent {
            resets_in: None,
            monthly: true,
        },
        ResetFmt::default(),
    )
    .iter()
    .map(|s| s.content.as_ref())
    .collect();
    assert_eq!(pill, "[ 30d spent ]");
}

/// The soft-block fix line names the monthly quota (`monthly quota past the
/// weekly switch line, still serving`), never the weekly wording — the owner's
/// copy ruling, pinned on the render arm itself.
#[test]
fn the_soft_fix_names_a_monthly_quota() {
    assert_eq!(
        reason_fix(
            &BlockedReason::WeeklySoft {
                pct: 99.0,
                monthly: true,
            },
            &crate::profile::ProfileName::from("acct"),
            false,
        ),
        "monthly quota past the weekly switch line, still serving"
    );
}

// A member with headroom shows no pill — the card opens straight on `5h usage`
// (chain position moved to the selector's `#n` rail, so the card no longer
// restates it).
#[test]
fn headroom_member_shows_no_reason_pill() {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let first = line_text(&lines[0]);
    assert!(
        !first.contains('['),
        "no pill for a member with headroom: {first:?}"
    );
    assert!(
        first.contains("5h usage"),
        "card opens on the 5h gauge: {first:?}"
    );
}

/// The first row's span `member_detail` RETURNS (`rows_start`, its start) must
/// be the index of the FIRST `FALLBACK_ROWS` row it actually pushed, at every
/// header height — 0 pills, 1 pill + its fix line, and the stacked 2. The spans
/// are what `draw_chain_detail` places the native caret with, so a drift puts a
/// typed field's caret on the wrong row, which no text-only assertion catches.
/// `rotate at` is the first `FALLBACK_ROWS` entry, so pinning
/// `rows_start == position_of("rotate at")` is the whole contract in one
/// equality — and it stays honest through a header-block change, unlike the
/// hand-maintained `ROWS_BEFORE` it replaced.
///
/// PILL heights only: this sweep hands `member_detail` a default
/// `MemberSessions`, so `live_session_lines` early-returns and the session
/// block never enters it. That axis is
/// `member_detail_rows_start_clears_the_session_block_at_every_height`.
#[test]
fn member_detail_rows_start_indexes_the_first_fallback_row_at_every_header_height() {
    let at_width = |cfg: &AppConfig, readings: &ReadingsSnapshot, width: usize| -> (usize, usize) {
        let (lines, spans, _) = member_detail(
            cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                width,
                ..Default::default()
            },
            &HashSet::new(),
            readings,
        );
        let first_row_at = lines
            .iter()
            .position(|l| line_text(l).contains("rotate at"))
            .expect("the first FALLBACK_ROWS row renders");
        (spans[0].start, first_row_at)
    };
    let start_and_first_row =
        |cfg: &AppConfig, readings: &ReadingsSnapshot| at_width(cfg, readings, 60);

    // 0 pills: gauge + headroom + blank only.
    let (healthy, readings) =
        config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let (start, row) = start_and_first_row(&healthy, &readings);
    assert_eq!(start, row, "no pill: rows_start indexes the first row");
    assert_eq!(start, 3, "gauge + headroom + blank");

    // 1 pill: adds the pill row, its `└` fix line, and the separating blank.
    let (one, readings) = config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    let (start, row) = start_and_first_row(&one, &readings);
    assert_eq!(start, row, "1 pill: rows_start indexes the first row");
    assert_eq!(start, 6, "pill + hint + blank on top of the 3 base rows");

    // 2 pills: disabled AND auth-broken stack, each with its own fix line.
    let (mut d, d_readings) = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let (mut two, readings) = config_with(vec![(d, d_readings)], Some("other"), vec!["a"]);
    two.state.auth_broken.push("a".into());
    let (start, row) = start_and_first_row(&two, &readings);
    assert_eq!(start, row, "2 pills: rows_start indexes the first row");
    assert_eq!(
        start, 8,
        "two pill+hint pairs + blank on top of the 3 base rows"
    );

    // Narrow: the SAME config must produce a taller header, because each fix
    // line now wraps. This is what makes `rows_start` load-bearing rather than a
    // function of the pill count — and it is what stops the caret test's
    // "wrapped" cases from being duplicates of its wide ones.
    let (narrow_start, narrow_row) = at_width(&two, &readings, 30);
    assert_eq!(
        narrow_start, narrow_row,
        "wrapped: rows_start still indexes the first row"
    );
    assert!(
        narrow_start > start,
        "a 30-col pane must wrap the fix lines and push the rows down \
         (wide={start}, narrow={narrow_start}) — otherwise nothing here tests wrapping"
    );
}

/// The caret math end-to-end, through the real `draw` path and the real
/// `frame.set_cursor_position` — the caret must land on the buffer row that
/// actually carries the `rotate at` field it is editing. Asserting against the
/// RENDERED row rather than an arithmetic delta is what makes this immune to
/// the header block changing height: it restates the user-visible contract
/// ("the caret is in the field") instead of re-deriving the implementation's
/// own sum. Driven at 0, 1 and 2 pills, and with the `priority` row gone.
///
/// The narrow cases are the whole reason `rows_start` exists: below ~40 columns
/// each fix line WRAPS, so the header block's height stops being a function of
/// the pill count alone. Kept tall (40 rows) so the row is always on-pane —
/// clipping is a separate contract, pinned by
/// `typed_threshold_caret_is_not_set_when_the_row_is_clipped_off_the_pane`.
#[test]
fn typed_threshold_caret_lands_on_the_rotate_at_row_at_every_header_height() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = |(cfg, readings): (AppConfig, ReadingsSnapshot), label: &str, w: u16, h: u16| {
        let mut app = app_with_readings(cfg, &readings);
        app.fallback_focus = FallbackFocus::Detail;
        app.fallback_detail_cursor = FALLBACK_ROWS
            .iter()
            .position(|r| *r == FallbackRow::Threshold)
            .unwrap();
        app.fallback_edit = Some(crate::tui::app::MemberEdit {
            member: "a".into(),
            state: CardEdit::Threshold(InputState::new("80")),
        });
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
            .unwrap();
        let caret = term
            .get_cursor_position()
            .expect("a typed field places the native caret");
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let rendered_at = rows
            .iter()
            .position(|r| r.contains("rotate at"))
            .unwrap_or_else(|| panic!("[{label}] rotate at renders:\n{}", rows.join("\n")));
        assert_eq!(
            caret.y as usize,
            rendered_at,
            "[{label}] caret must sit on the rotate-at row, not {} rows off",
            (caret.y as i64) - (rendered_at as i64)
        );
    };

    let healthy = || config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let one_pill = || config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    // 2 pills: disabled AND auth-broken, each with its own fix line.
    let two_pills = || {
        let (mut d, d_readings) = profile("a", 95.0, 10.0, 7200);
        d.disabled = true;
        let (mut two, readings) = config_with(vec![(d, d_readings)], Some("other"), vec!["a"]);
        two.state.auth_broken.push("a".into());
        (two, readings)
    };

    check(healthy(), "0 pills", 120, 30);
    check(one_pill(), "1 pill", 120, 30);
    check(two_pills(), "2 pills", 120, 30);

    // Wrapped hints: one fix line becomes 2-3 rows, so the header height is no
    // longer derivable from the pill count.
    check(one_pill(), "1 pill, wrapped", 34, 40);
    check(two_pills(), "2 pills, wrapped", 34, 40);
    check(two_pills(), "2 pills, hard-wrapped", 26, 40);
}

/// A short pane can no longer strand a typed row: the focused card SCROLLS the
/// cursored row into view (the header block alone can outgrow the pane — two
/// pills, each dragging a fix line that wraps), and the caret math subtracts
/// the same scroll, so the caret lands on the row's RENDERED position rather
/// than its absolute index. `set_cursor_position` takes an absolute row and a
/// real terminal clamps an out-of-range one onto the last line, so the caret
/// must also never be set past the pane.
#[test]
fn typed_threshold_row_scrolls_into_view_and_carries_the_caret() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut d, d_readings) = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let (mut cfg, _readings) = config_with(vec![(d, d_readings)], Some("other"), vec!["a"]);
    cfg.state.auth_broken.push("a".into());

    let mut app = App::new(cfg);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Threshold)
        .unwrap();
    app.fallback_edit = Some(crate::tui::app::MemberEdit {
        member: "a".into(),
        state: CardEdit::Threshold(InputState::new("80")),
    });

    // 26x17: without the scroll, the two wrapped fix lines pushed the rows
    // past the pane's last line (the pre-scroll revision of this test pinned
    // exactly that clip).
    let (w, h) = (26u16, 17u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();

    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let rendered_at = rows
        .iter()
        .position(|r| r.contains("rotate at"))
        .unwrap_or_else(|| {
            panic!(
                "the typed row must scroll into view on a short pane:\n{}",
                rows.join("\n")
            )
        });
    let caret = term.get_cursor_position().unwrap();
    assert_eq!(
        caret.y as usize, rendered_at,
        "caret must sit on the row's rendered (scrolled) position",
    );
    assert!(
        (caret.y as usize) < h as usize,
        "caret must never be set past the terminal's last row (y={}, h={h})",
        caret.y
    );
}

/// A custom day list that wraps at rest pushes every row beneath it down a
/// line, and the `max spend` caret follows: it lands on the row's rendered
/// line, not on the line its `FALLBACK_ROWS` index alone would name.
#[test]
fn typed_max_spend_caret_lands_on_its_row_under_a_wrapped_day_list() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let (mut a, a_readings) = profile("a", 95.0, 10.0, 7200);
    a.preferred_days = vec![Mon, Tue, Wed, Thu, Fri, Sat];
    let (config, _readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
    let mut app = App::new(config);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::MaxSpend)
        .unwrap();
    app.fallback_edit = Some(crate::tui::app::MemberEdit {
        member: "a".into(),
        state: CardEdit::MaxSpend(InputState::new("5")),
    });

    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let day_row = rows
        .iter()
        .position(|r| r.contains("preferred days"))
        .expect("the day row renders");
    let rendered_at = rows
        .iter()
        .position(|r| r.contains("max spend"))
        .expect("the max spend row renders");
    assert!(
        rows[day_row + 1..rendered_at]
            .iter()
            .any(|r| r.contains("thu, fri, sat")),
        "precondition: the day row wraps onto lines of its own:\n{}",
        rows.join("\n")
    );
    let caret = term.get_cursor_position().unwrap();
    assert_eq!(
        caret.y as usize, rendered_at,
        "caret must sit on the max spend row"
    );
}

/// Finding shape from review round 2: a 40x24 terminal (~14 inner rows after
/// borders and the left chain) with a blocked member fills the card past the
/// pane — the LAST rows must stay reachable by walking the cursor down, not
/// fall off the bottom.
#[test]
fn remove_row_stays_reachable_on_a_40x24_pane() {
    let _home = crate::testutil::HomeSandbox::new();
    // One blocked member: the pill block + fix line eat header rows.
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    let mut app = app_with_readings(cfg, &readings);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Remove)
        .unwrap();

    let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|r| r.contains("remove")),
        "the cursored last row must scroll into view at 40x24:\n{}",
        rows.join("\n")
    );
}

/// The Fallback detail card keeps BOTH facts for a disabled member: the
/// `[ disabled ]` pill says the operator excluded it, the health pill beneath
/// says it is also broken. Before this, `Disabled` ranking first meant the card
/// showed only the exclusion and the dead login was invisible tab-wide.
#[test]
fn member_detail_stacks_the_health_pill_under_disabled() {
    let (mut d, d_readings) = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let (mut cfg, readings) = config_with(vec![(d, d_readings)], Some("other"), vec!["a"]);
    cfg.state.auth_broken.push("a".into());

    // Both pills on one `├│└` rail, each with its own fix line. The first row
    // carries the `status` key so the rail has a column to anchor against; the
    // second bridges with `│` at col 0 while the rail is still open.
    let (lines, _, _) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    );
    let block: Vec<String> = lines.iter().take(4).map(line_text).collect();
    assert_eq!(
        block,
        vec![
            "status          [ disabled ]".to_string(),
            "├ excluded from the walk, enable it on the setup tab".to_string(),
            "│               [ auth broken ]".to_string(),
            "└ re-login with clauth login a".to_string(),
        ],
        "both facts stack on one rail, each naming its own fix"
    );

    // An ENABLED but auth-broken member is unchanged: one pill, lone `└`.
    let (mut e, e_readings) = profile("a", 95.0, 10.0, 7200);
    e.disabled = false;
    let (mut enabled, readings) = config_with(vec![(e, e_readings)], Some("other"), vec!["a"]);
    enabled.state.auth_broken.push("a".into());
    let (lines, _, _) = member_detail(
        &enabled,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    );
    assert_eq!(
        lines.iter().take(2).map(line_text).collect::<Vec<_>>(),
        vec![
            "status          [ auth broken ]".to_string(),
            "└ re-login with clauth login a".to_string(),
        ],
        "a single pill stays a lone `└` — nothing to connect"
    );
}

/// The selector rail shows `#n` and keeps a CONSTANT width across one- and
/// two-digit positions, so every name in the list starts on the same column.
/// A ragged rail is what a bare `{}` would give, and it is invisible in any
/// test that only renders a short chain.
#[test]
fn selector_rail_shows_hash_n_at_constant_width() {
    let _home = crate::testutil::HomeSandbox::new();
    // 10 members so the list spans `#1` through `#10`. Names are deliberately
    // non-prefixing (`ma`..`mj`, never `m1`..`m10`): `m1` is a substring of `m10`
    // and `#1` of `#10`, so a naive `contains` lookup would match the wrong row
    // and then measure a column that happens to agree anyway.
    let names: Vec<String> = (b'a'..=b'j').map(|c| format!("m{}", c as char)).collect();
    let profiles: Vec<_> = names.iter().map(|n| profile(n, 95.0, 10.0, 7200)).collect();
    let chain: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let (cfg, _readings) = config_with(profiles, None, chain);
    let app = App::new(cfg);

    let (w, h) = (100u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    // Both panes share every physical row and the detail card repeats the
    // selected member's name in its TITLE (on a border row, which carries no
    // `││` seam to split on) — so identify selector rows by the rail's own `#`
    // rather than by name, and measure the name column within those.
    // Walk forward from the rail's own `#` (past its digits and the gap) to find
    // where the name starts. Searching the row for the name STRING instead would
    // collide with the detail pane sharing the same physical row — `max spend`
    // contains `ma`, which silently reported a name column of 34.
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let mut name_cols = Vec::new();
    for row in &rows {
        let chars: Vec<char> = row.chars().collect();
        let Some(hash) = chars.iter().position(|c| *c == '#') else {
            continue;
        };
        let mut i = hash + 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        // Our members are the only `m?` tokens that can follow a rail ordinal.
        if chars.get(i) == Some(&'m') {
            name_cols.push(i);
        }
    }
    assert_eq!(
        name_cols.len(),
        names.len(),
        "every member must render a `#n` selector row; got {name_cols:?}"
    );
    assert_eq!(
        name_cols
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "every name must start on the same column; got {name_cols:?}"
    );

    // …and the ordinals really are `#n`, spanning both digit widths — otherwise
    // a rail that dropped the `#` entirely would still pass the column check.
    let joined = rows.join("\n");
    assert!(joined.contains("#1"), "first position renders as #1");
    assert!(joined.contains("#10"), "tenth position renders as #10");
}

/// Every `reason_fix` arm must be reachable and non-empty: a match arm that can
/// never fire is dead copy, and an empty hint renders a bare `└` with nothing
/// after it. Enumerates one representative of each variant.
#[test]
fn every_reason_fix_variant_is_reachable_and_non_empty() {
    let all = [
        BlockedReason::Disabled,
        BlockedReason::Canceled,
        BlockedReason::AuthBroken,
        BlockedReason::KeyRejected,
        BlockedReason::WeeklySpent {
            resets_in: None,
            monthly: false,
        },
        BlockedReason::KickRejected { lifts_in: 60 },
        BlockedReason::BudgetSpent,
        BlockedReason::FiveHour {
            pct: 99.0,
            resets_in: None,
        },
        BlockedReason::WeeklySoft {
            pct: 85.0,
            monthly: false,
        },
        BlockedReason::Stale,
        BlockedReason::NoUsage,
    ];
    let mut seen: Vec<String> = Vec::new();
    for reason in all {
        let fix = reason_fix(&reason, &crate::profile::ProfileName::from("acct"), false);
        assert!(!fix.trim().is_empty(), "{reason:?} has no fix copy");
        assert_eq!(
            fix,
            fix.to_lowercase(),
            "{reason:?} fix must stay lowercase like every other hint: {fix}"
        );
        assert!(
            !seen.contains(&fix),
            "{reason:?} reuses another variant's copy ({fix}) — the arm is indistinguishable"
        );
        seen.push(fix);
    }
    assert_eq!(seen.len(), 11, "every variant contributed a distinct fix");
}

/// F2: the key-rejected copy is cloudy's verbatim, pinned by equality — the
/// `reason_fix` line, the `key rejected` pill label, and the rendered `[ … ]`
/// pill. A copy typo must red, not ride a reachability/nonempty check.
#[test]
fn key_rejected_copy_is_pinned_by_equality() {
    let fix = reason_fix(
        &BlockedReason::KeyRejected,
        &crate::profile::ProfileName::from("acct"),
        false,
    );
    assert_eq!(
        fix, "re-enter the api key on the setup tab",
        "the KeyRejected fix line is cloudy's verbatim"
    );
    assert_eq!(
        super::DIAG_KEY_REJECTED,
        "key rejected",
        "the pill label is cloudy's verbatim"
    );
    let pill: String = reason_pill_spans(&BlockedReason::KeyRejected, ResetFmt::default())
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(pill, "[ key rejected ]", "the rendered pill is {pill:?}");
}

// ── `max spend` dims while inert (spend budget off) ──────────────────────────

fn span_style(line: &Line<'static>, needle: &str) -> Option<ratatui::style::Style> {
    line.spans
        .iter()
        .find(|s| s.content.contains(needle))
        .map(|s| s.style)
}

// A set ceiling with the chain-wide `spend budget` OFF spends nothing, so it must
// not read as armed: render the value faint. Flip spend budget on and the same
// ceiling renders in ACCENT.
#[test]
fn max_spend_dims_when_spend_budget_is_off() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (mut cfg, readings) =
        config_with(vec![profile("a", 95.0, 40.0, 3600)], Some("a"), vec!["a"]);
    cfg.profiles[0].max_auto_spend = Some(25.0);

    let off = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 2,
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let off_val = off
        .iter()
        .find_map(|l| span_style(l, "$25.00"))
        .expect("max spend ceiling renders");
    assert_eq!(
        off_val.fg,
        crate::tui::theme::faint().fg,
        "an inert ceiling (spend budget off) renders faint"
    );

    cfg.state.spend_budget_switching = true;
    let on = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            focused: true,
            row_cursor: 2,
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let on_val = on
        .iter()
        .find_map(|l| span_style(l, "$25.00"))
        .expect("max spend ceiling renders");
    assert_eq!(
        on_val.fg,
        crate::tui::theme::accent().fg,
        "an armed ceiling (spend budget on) renders in accent"
    );
}

// The `weekly at` row reads as a state: the chain default faint when unset, a
// member-set figure in accent with the default alongside, and the whole row
// dimmed (inert) while the member's weekly gate is off.
#[test]
fn weekly_at_row_distinguishes_default_override_and_gated_off() {
    let row_texts = |(p, p_readings): (Profile, ReadingsSnapshot)| -> Vec<String> {
        let (cfg, readings) = config_with(vec![(p, p_readings)], Some("a"), vec!["a"]);
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: 1,
                width: 80,
                ..Default::default()
            },
            &HashSet::new(),
            &readings,
        )
        .0
        .iter()
        .map(line_text)
        .collect()
    };

    let unset = row_texts(profile("a", 95.0, 20.0, 3600));
    let row = unset
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(row.contains("98%"), "{row}");
    assert!(
        !row.contains("chain default"),
        "unset row must not restate the deleted chain-default label: {row}"
    );
    assert!(
        unset
            .iter()
            .any(|t| t
                .contains("switches away from this account at the chain's shared weekly level")),
        "{unset:?}"
    );

    let (mut p, p_readings) = profile("a", 95.0, 20.0, 3600);
    p.weekly_threshold = Some(90.0);
    let set = row_texts((p, p_readings));
    let row = set
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(row.contains("90%"), "{row}");
    assert!(row.contains("default:"), "{row}");
    assert!(
        set.iter()
            .any(|t| t
                .contains("switches away from this account once weekly usage passes this level")),
        "{set:?}"
    );

    let (mut p, p_readings) = profile("a", 95.0, 20.0, 3600);
    p.check_weekly = false;
    let gated = row_texts((p, p_readings));
    assert!(
        gated
            .iter()
            .any(|t| t.contains("weekly gate is off, this line isn't checked")),
        "{gated:?}"
    );
}

// `weekly at`'s "default: N%" reminder must mirror `rotate at`: render only
// when the override actually differs from the chain default, not
// unconditionally (it used to always render once a value was set).
#[test]
fn weekly_at_default_reminder_only_shows_when_value_differs_from_default() {
    let row_texts = |(p, p_readings): (Profile, ReadingsSnapshot)| -> Vec<String> {
        let (cfg, readings) = config_with(vec![(p, p_readings)], Some("a"), vec!["a"]);
        member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: 1,
                width: 80,
                ..Default::default()
            },
            &HashSet::new(),
            &readings,
        )
        .0
        .iter()
        .map(line_text)
        .collect()
    };

    // Chain-wide default is 98% (`DEFAULT_WEEKLY_SWITCH_PCT`); an override set
    // to exactly that value must not carry the reminder.
    let (mut at_default, at_default_readings) = profile("a", 95.0, 20.0, 3600);
    at_default.weekly_threshold = Some(98.0);
    let lines = row_texts((at_default, at_default_readings));
    let row = lines
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(row.contains("98%"), "{row}");
    assert!(
        !row.contains("default:"),
        "no reminder when the override equals the chain default: {row}"
    );

    let (mut off_default, off_default_readings) = profile("a", 95.0, 20.0, 3600);
    off_default.weekly_threshold = Some(90.0);
    let lines = row_texts((off_default, off_default_readings));
    let row = lines
        .iter()
        .find(|t| t.contains("weekly at"))
        .expect("weekly at row renders");
    assert!(
        row.contains("default:"),
        "reminder renders when the override differs from the chain default: {row}"
    );
}

// Bug 6 class fix: the edit-mode glyph pairs ACCENT + bold, matching the
// selection caret `❯` — this card rendered it accent-only.
#[test]
fn edit_glyph_is_bold_like_the_selection_caret() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let input = InputState::new("80");
    let lines = detail_row(
        FallbackRow::Threshold,
        true,
        MemberRow {
            threshold: 80.0,
            weekly_override: None,
            weekly_default: 98.0,
            check_weekly: true,
            check_scoped: true,
            check_credit_cap: true,
            last_resort: false,
            preferred: false,
            preferred_days: &[],
            day_picker: None,
            day_stop: None,
            max_spend: 0.0,
            spend_budget: false,
            armed_remove: false,
        },
        Some(&input),
        60,
    );
    let glyph = &lines[0].spans[0];
    assert!(
        glyph
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "edit glyph must be bold: {glyph:?}"
    );
    assert_eq!(
        glyph.style.fg,
        theme::accent().fg,
        "edit glyph stays accent"
    );
}

// ── live sessions on the chain ───────────────────────────────────────────────

/// One live session's registry row, running as the account it launched on. The
/// swap attribution itself is pinned in `live_sessions.rs`'s own tests.
fn live_row(
    session_id: &str,
    member: &str,
    follows_chain: bool,
    last_swap_at: Option<u64>,
) -> crate::live_sessions::LiveSession {
    crate::live_sessions::LiveSession {
        follows_chain,
        last_swap_at,
        ..crate::testutil::live_row(session_id, member)
    }
}

/// The tally renders in the DETAIL PANE ONLY (owner call, 2026-07-26): it used
/// to ride the selector row too, so one number appeared twice on one screen and
/// the compact copy was the one that could not explain itself. The selector row
/// carries the member's name and its blocked-reason marker, nothing else.
///
/// This is a DELETION pin, so presence assertions cannot guard it: a retargeted
/// `.contains` stays green the moment the span comes back. The pane is asserted
/// whole, plus an explicit absence of the follower glyph.
#[test]
fn a_chain_row_carries_no_live_session_tally() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) = config_with(
        vec![
            profile("busy", 95.0, 10.0, 3600),
            profile("idle", 95.0, 10.0, 3600),
        ],
        None,
        vec!["busy", "idle"],
    );
    let mut app = App::new(config);
    app.live_sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "busy", true, None),
        live_row("4242-1", "busy", false, None),
    ]);

    let pane = chain_selector_pane(&app, 100, 8);
    assert_eq!(
        pane,
        vec![
            "╭─ CHAIN ────────────────────╮".to_string(),
            "│ ❯  #1 busy                 │".to_string(),
            "│    #2 idle                 │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "╰────────────────────────────╯".to_string(),
        ],
    );
    assert!(
        !pane.iter().any(|row| row.contains('⇄')),
        "the follower mark belongs to the detail pane alone:\n{}",
        pane.join("\n")
    );
}

/// Every row of the Fallback tab's chain selector pane, borders included, sliced
/// off the rendered frame at the pane's own width. Returned whole so a pin is an
/// exact vec rather than a substring probe — a `.contains` on one row cannot see
/// a glyph that got appended past the pane and clipped.
fn chain_selector_pane(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &app.readings_snapshot(), &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let pane_w = rows
        .iter()
        .find_map(|r| r.find('╮').map(|b| r[..b].chars().count() + 1))
        .expect("the chain pane's top-right corner");
    rows.iter()
        .map(|r| r.chars().take(pane_w).collect())
        .collect()
}

/// Text of every line in the member card, trimmed, for exact-vec assertions.
fn card_texts(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The block, whole, for a member hosting sessions none of which has swapped:
/// the count under the app-wide `live` key with the follower qualifier (since
/// one of the two rides the chain), and NOTHING else. A session that never
/// swapped has no repointed credential link, so §12's pickup lag does not apply
/// to it and claiming it would over-warn in the other direction.
#[test]
fn the_session_block_emits_the_follower_qualifier_when_one_session_follows() {
    let sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "a", true, None),
        live_row("4242-1", "a", false, None),
    ])
    .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec!["live            2, 1 with fallback".to_string()],
    );
}

/// A pure-pinned count has no movable split to name, so the qualifier stays
/// off. `following > 0` is the gate, not `sessions > 1`. Pairs with the
/// mixed-fixture test above to pin both arms of the branch.
#[test]
fn the_session_block_omits_the_qualifier_when_no_session_follows() {
    let sessions = crate::live_sessions::LiveTally::of([
        live_row("4242-0", "a", false, None),
        live_row("4242-1", "a", false, None),
    ])
    .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec!["live            2".to_string()],
    );
}

/// Once a session HAS swapped onto this member, `current_member` stops being an
/// instantaneous fact: Claude Code re-reads its credentials on its next request
/// and nothing observes the pickup.
/// The card says so rather than inventing a "not yet picked up" state the
/// registry cannot see.
#[test]
fn the_session_block_dates_the_last_swap_and_says_when_it_is_picked_up() {
    // Mid-unit (3h30m), so the wall clock cannot walk the fixture across a
    // `relative_age` boundary between building it and rendering it.
    let swapped_at = crate::usage::now_ms().saturating_sub(3 * 3_600_000 + 1_800_000);
    let sessions =
        crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, Some(swapped_at))])
            .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec![
            "live            1, 1 with fallback".to_string(),
            "last swap       3h ago".to_string(),
            " └ picked up on the session's next request".to_string(),
        ],
    );
}

/// A swap that landed inside the second the card renders reads `just now`.
/// `relative_age` owns that arm, which is what retired the `max(1)` guard the
/// old two-unit formatter needed to keep from rendering the phrase `now ago`.
#[test]
fn a_swap_this_very_second_reads_as_just_now() {
    let sessions = crate::live_sessions::LiveTally::of([live_row(
        "4242-0",
        "a",
        false,
        Some(crate::usage::now_ms()),
    )])
    .member(&crate::profile::ProfileName::from("a"));

    assert_eq!(
        card_texts(&live_session_lines(sessions, 60)),
        vec![
            "live            1".to_string(),
            "last swap       just now".to_string(),
            " └ picked up on the session's next request".to_string(),
        ],
    );
}

/// The age line follows the time-formatting rule: ONE unit, the
/// largest that is at least 1, and the absolute ISO date at 30 days and beyond.
/// The two-unit `humanize_duration` the countdowns use would render `1d 4h ago`
/// here and never reach a date at all — it stays on the countdowns, where a
/// duration is what is being shown.
///
/// Every relative fixture sits MID-unit so the wall clock cannot walk it across
/// a boundary mid-test; the date case uses a fixed epoch, so its expectation
/// derives through chrono's own `format` rather than a literal — a literal would
/// tie the pin to the runner's zone, and the second derivation is what keeps it
/// a claim about the code instead of a copy of the code's arithmetic.
#[test]
fn the_last_swap_age_renders_one_unit_and_a_date_past_thirty_days() {
    let age_line = |at: u64| -> String {
        let sessions =
            crate::live_sessions::LiveTally::of([live_row("4242-0", "a", false, Some(at))])
                .member(&crate::profile::ProfileName::from("a"));
        card_texts(&live_session_lines(sessions, 60))
            .into_iter()
            .find(|t| t.starts_with("last swap"))
            .expect("the block dates a swap")
            .trim_start_matches("last swap")
            .trim()
            .to_string()
    };
    let ago = |ms: u64| crate::usage::now_ms().saturating_sub(ms);

    assert_eq!(age_line(ago(30_000)), "just now");
    assert_eq!(age_line(ago(5 * 60_000 + 30_000)), "5m ago");
    assert_eq!(age_line(ago(2 * 3_600_000 + 1_800_000)), "2h ago");
    assert_eq!(age_line(ago(3 * 86_400_000 + 43_200_000)), "3d ago");
    assert_eq!(age_line(ago(12 * 86_400_000)), "1w ago");
    // 2023-11-14T22:13:20Z — permanently past 30 days, so the arm is the local
    // ISO date, derived through chrono's own `format` (a second derivation of
    // the same claim), so the pin holds in any zone, UTC included.
    let expected = chrono::DateTime::from_timestamp(1_700_000_000, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(age_line(1_700_000_000_000), expected);
}

/// An account hosting nothing says nothing — no row at all rather than
/// `sessions 0`, matching the Overview cell's blank and the selector's.
#[test]
fn the_session_block_is_absent_when_nothing_is_live() {
    assert_eq!(
        card_texts(&live_session_lines(
            crate::live_sessions::MemberSessions::default(),
            60
        )),
        Vec::<String>::new(),
    );
}

/// Position and rhythm: the block sits ABOVE the 5h gauge with its own
/// trailing blank separating the two. The row spans are read off the buffer,
/// so a block inserted anywhere else would move the native caret off every
/// row they point at.
#[test]
fn the_member_card_places_the_session_block_above_the_five_hour_gauge() {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 10.0, 3600)], None, vec!["a"]);
    let sessions = crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, None)])
        .member(&crate::profile::ProfileName::from("a"));

    let (lines, spans, _) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            sessions,
            width: 60,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    );
    let texts = card_texts(&lines);

    assert_eq!(
        &texts[..spans[0].start],
        [
            "live            1, 1 with fallback",
            "",
            "5h usage        ██░░░░░░░░░░░░░░░░░░░│  10% used",
            "                85% until rotate",
            "",
        ],
    );
}

/// A narrow card reflows the `5h usage` figure to its own line once the gauge
/// floor leaves no room, instead of clipping `% used` off the pane edge.
#[test]
fn the_five_hour_figure_drops_to_its_own_line_when_the_gauge_leaves_no_room() {
    // The expected rows spell the full tier's toggle glyphs (`─●` / `○─`).
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 49.0, 3600)], None, vec!["a"]);
    let (lines, _, _) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 24,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    );
    let texts = card_texts(&lines);
    assert_eq!(
        texts,
        [
            "5h usage        ████░░░│",
            "                49% used",
            "                46% unt…",
            "",
            "  rotate at       95%",
            "  weekly at       98%",
            "  weekly gate     ─●",
            "  scoped gate     ─●",
            "  credit gate     ─●",
            "  last resort     ○─",
            "  preferred       ○─",
            "  preferred days",
            "  never  weekdays",
            "  weekends  every day",
            "  max spend       off",
            "  remove",
            "  remove from chain",
        ]
    );
}

/// The member card's toggle rows take the compatible tier's `[on]` / `[off]`
/// on a terminal without truecolor, never the full tier's glyphs.
#[test]
fn the_member_card_toggle_rows_read_on_off_on_the_compatible_tier() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 49.0, 3600)], None, vec!["a"]);
    let (lines, _, _) = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 24,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    );
    assert_eq!(
        &card_texts(&lines)[6..11],
        [
            "  weekly gate     [on]",
            "  scoped gate     [on]",
            "  credit gate     [on]",
            "  last resort     [off]",
            "  preferred       [off]",
        ]
    );
}

/// A leg nothing calls is a leg that passes every unit test and ships nothing:
/// the member card has to read the app's own tally, not a default. The selector
/// pane is asserted alongside it so the tally's absence there is pinned on the
/// SAME fixture that proves the tally is non-empty.
#[test]
fn the_fallback_tab_reads_the_apps_live_session_tally() {
    let _home = crate::testutil::HomeSandbox::new();
    let (config, _readings) =
        config_with(vec![profile("busy", 95.0, 10.0, 3600)], None, vec!["busy"]);
    let mut app = App::new(config);
    app.live_sessions =
        crate::live_sessions::LiveTally::of([live_row("4242-0", "busy", true, None)]);

    // The selector pane exactly, and the card's own row exactly. The card
    // reading `Default::default()` instead of the app's tally leaves every unit
    // test above it green.
    assert_eq!(
        chain_selector_pane(&app, 120, 6),
        vec![
            "╭─ CHAIN ──────────────────────────╮".to_string(),
            "│ ❯  #1 busy                       │".to_string(),
            "│                                  │".to_string(),
            "│                                  │".to_string(),
            "│                                  │".to_string(),
            "╰──────────────────────────────────╯".to_string(),
        ],
    );

    let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &app.readings_snapshot(), &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    // The padded key cell, not the bare word: `live` is short enough that a
    // frame-wide search for it would match any prose on the screen.
    const KEY: &str = "live            ";
    let card = rows
        .iter()
        .find(|r| r.contains(KEY))
        .unwrap_or_else(|| panic!("the card carries no session row:\n{}", rows.join("\n")));
    assert_eq!(
        card.split('│')
            .find(|seg| seg.contains(KEY))
            .map(str::trim_end),
        Some(" live            1, 1 with fallback"),
    );
}

/// Three members, one healthy and active, one disabled, one out of 5h headroom
/// — the fixture both marker pins run on. `blockedname` is 11 chars so it
/// overruns the narrow pane's name budget while `hot` still fits it.
fn marker_app() -> App {
    let (mut off, off_readings) = profile("blockedname", 95.0, 10.0, 7200);
    off.disabled = true;
    let (cfg, readings) = config_with(
        vec![
            profile("ok", 95.0, 10.0, 7200),
            (off, off_readings),
            profile("hot", 50.0, 99.0, 7200),
        ],
        Some("ok"),
        vec!["ok", "blockedname", "hot"],
    );
    app_with_readings(cfg, &readings)
}

/// The blocked-reason marker owns the row's LAST content column, and nothing
/// else pins that. Both surviving `chain_selector_pane` assertions run on
/// fixtures whose members are all healthy, so `if let Some(reason)` never
/// executes in either and the right-align arithmetic is free to drift; the pin
/// that covered it incidentally went out with its own subject in `94e0245`.
///
/// The pane is asserted WHOLE, because a `.contains` on a row cannot see a
/// marker that got appended past the pane and clipped. This is the wide case,
/// where every name fits and a drift reads as a column shift; the narrow case
/// is its own pin below.
#[test]
fn the_blocked_reason_marker_holds_the_rows_last_content_column() {
    let _home = crate::testutil::HomeSandbox::new();
    // 26 content cells: `⊖` and `◔` both sit one cell in from the pane's right
    // border, and the healthy active member carries no marker at all.
    assert_eq!(
        chain_selector_pane(&marker_app(), 100, 8),
        vec![
            "╭─ CHAIN ────────────────────╮".to_string(),
            "│ ❯  #1 ok                   │".to_string(),
            "│    #2 blockedname        ⊖ │".to_string(),
            "│    #3 hot                ◔ │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "│                            │".to_string(),
            "╰────────────────────────────╯".to_string(),
        ],
    );
}

/// The selector marks a member clauth cannot measure with `◆` in the same
/// last content column every other marker holds. The fixture installs the
/// sidecar in the app's own cache, which is where the render path reads it: a
/// `NotLongLived` mis-fill is NOT this shape (the split stays disengaged, so
/// `credentials.json` is still what switches install), and a member with no
/// sidecar at all is the logged-out shape, not this one.
#[test]
fn the_selector_marks_a_member_clauth_cannot_measure() {
    let _home = crate::testutil::HomeSandbox::new();
    let pane = |status: Option<crate::claude::SessionTokenStatus>| {
        let (blind, _unpolled) = profile("blind", 95.0, 0.0, 7200);
        let (cfg, readings) = config_with(
            vec![
                profile("ok", 95.0, 10.0, 7200),
                (blind, ReadingsSnapshot::default()),
            ],
            Some("ok"),
            vec!["ok", "blind"],
        );
        let mut app = app_with_readings(cfg, &readings);
        if let Some(status) = status {
            app.session_tokens.insert("blind".into(), status);
        }
        chain_selector_pane(&app, 100, 5)
    };
    assert_eq!(
        pane(Some(crate::claude::SessionTokenStatus::LongLived(None))),
        vec![
            "╭─ CHAIN ────────────────────╮".to_string(),
            "│ ❯  #1 ok                   │".to_string(),
            "│    #2 blind              ◆ │".to_string(),
            "│                            │".to_string(),
            "╰────────────────────────────╯".to_string(),
        ],
    );
    let unmarked = vec![
        "╭─ CHAIN ────────────────────╮".to_string(),
        "│ ❯  #1 ok                   │".to_string(),
        "│    #2 blind                │".to_string(),
        "│                            │".to_string(),
        "╰────────────────────────────╯".to_string(),
    ];
    assert_eq!(pane(None), unmarked, "no sidecar, no mark");
    assert_eq!(
        pane(Some(crate::claude::SessionTokenStatus::NotLongLived)),
        unmarked,
        "a mis-fill installs nothing, so it is not this shape either"
    );
}

/// The narrow counterpart: a name too long for the row gives up the cells the
/// marker needs, rather than the marker being appended past the pane for
/// ratatui to discard — which rendered a BLOCKED account identically to a
/// healthy one. The clamp is charged ONLY to rows carrying a marker, so
/// `blockedname` truncates at 8 cells while unmarked names keep their full
/// width; that is why the name column's width tracks blocked state.
#[test]
fn a_narrow_pane_clamps_a_marked_members_name_rather_than_dropping_its_marker() {
    let _home = crate::testutil::HomeSandbox::new();
    // 16 content cells: the rail eats 6, the marker and its 3-cell gap 4,
    // leaving 6 for the name — so `blockedname` truncates with `…` and keeps
    // its `⊖`, while `hot` fits with room to spare.
    assert_eq!(
        chain_selector_pane(&marker_app(), 60, 8),
        vec![
            "╭─ CHAIN ──────────╮".to_string(),
            "│ ❯  #1 ok         │".to_string(),
            "│    #2 block…   ⊖ │".to_string(),
            "│    #3 hot      ◔ │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "╰──────────────────╯".to_string(),
        ],
    );
}

/// `rows_start` (the first row span's start) against the SESSION BLOCK, which
/// the header-height sweep above cannot see: it passes a default
/// `MemberSessions`, so `live_session_lines` early-returns and contributes
/// nothing. The block is pushed immediately BEFORE the rows, so its 2-to-5
/// lines move the anchor as surely as a pill does — and a caret on the wrong
/// row is invisible to every text assertion, which is the whole reason the
/// spans are read off the buffer.
///
/// Three heights, one per thing the block can add: the count alone, the count
/// plus a dated swap and its always-on caveat, and that caveat WRAPPED on a
/// narrow pane. The swap stamp sits mid-unit so the wall clock cannot walk the
/// fixture across a `relative_age` boundary mid-test.
#[test]
fn member_detail_rows_start_clears_the_session_block_at_every_height() {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 10.0, 7200)], None, vec!["a"]);
    let start_and_first_row =
        |sessions: crate::live_sessions::MemberSessions, width: usize| -> (usize, usize, usize) {
            let (lines, spans, _) = member_detail(
                &cfg,
                &crate::profile::ProfileName::from("a"),
                MemberCard {
                    sessions,
                    width,
                    ..Default::default()
                },
                &HashSet::new(),
                &readings,
            );
            let first_row_at = lines
                .iter()
                .position(|l| line_text(l).contains("rotate at"))
                .expect("the first FALLBACK_ROWS row renders");
            (spans[0].start, first_row_at, lines.len())
        };

    // Baseline: no live session, so the block contributes nothing.
    let (bare, row, _) = start_and_first_row(Default::default(), 60);
    assert_eq!(bare, row, "no sessions: rows_start indexes the first row");
    assert_eq!(bare, 3, "gauge + headroom + blank");

    // Count only: the block's own leading blank plus the `live` row.
    let counted = crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, None)])
        .member(&crate::profile::ProfileName::from("a"));
    let (start, row, _) = start_and_first_row(counted, 60);
    assert_eq!(start, row, "count only: rows_start indexes the first row");
    assert_eq!(start, bare + 2, "blank + the `live` row");

    // Plus a dated swap, which always drags its caveat line.
    let swapped_at = crate::usage::now_ms().saturating_sub(3 * 3_600_000 + 1_800_000);
    let dated =
        crate::live_sessions::LiveTally::of([live_row("4242-0", "a", true, Some(swapped_at))])
            .member(&crate::profile::ProfileName::from("a"));
    let (start, row, _) = start_and_first_row(dated, 60);
    assert_eq!(start, row, "dated swap: rows_start indexes the first row");
    assert_eq!(start, bare + 4, "blank + `live` + `last swap` + the caveat");

    // The caveat wrapped: the SAME tally must push the rows further down on a
    // narrow pane, which is what makes `rows_start` load-bearing rather than a
    // function of the block's shape.
    let (narrow, narrow_row, _) = start_and_first_row(dated, 30);
    assert_eq!(
        narrow, narrow_row,
        "wrapped caveat: rows_start still indexes the first row"
    );
    assert!(
        narrow > start,
        "a 30-col pane must wrap the caveat and push the rows down \
         (wide={start}, narrow={narrow}) — otherwise nothing here tests wrapping"
    );
}

// ── the `preferred days` row ─────────────────────────────────────────────────

/// The card with the cursor parked on `preferred days`, handing back that row's
/// own line. Read by whole-line equality because `preferred` is a prefix of this
/// key — anything looser lets the toggle row answer for it.
fn preferred_days_line(cfg: &AppConfig) -> Line<'static> {
    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::PreferredDays)
        .expect("the row exists");
    let lines = member_detail(
        cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            row_cursor: row,
            width: 80,
            ..Default::default()
        },
        &HashSet::new(),
        &ReadingsSnapshot::default(),
    )
    .0;
    lines
        .iter()
        .find(|l| line_text(l).contains("preferred days"))
        .cloned()
        .expect("the preferred days row renders")
}

/// Every rung the row can hold, the preset run whole, the held one ACCENT and
/// the rest TEXT_FAINT. `never` is a live value like any other rung, so it
/// lights ACCENT too; a custom set trails the run in ACCENT with every preset
/// faint.
#[test]
fn preferred_days_row_lights_the_held_rung_never_included() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use chrono::Weekday::*;
    const RUN: &str = "  preferred days  never  weekdays  weekends  every day";
    // A hand-written list is read order-blind, so `sun, sat` is the `weekends`
    // rung whatever order the file wrote it in.
    let cases: [(Vec<chrono::Weekday>, &str, &[&str]); 6] = [
        (Vec::new(), "", &["never"]),
        (vec![Mon, Tue, Wed, Thu, Fri], "", &["weekdays"]),
        (vec![Sat, Sun], "", &["weekends"]),
        (WEEKDAYS_ALL.to_vec(), "", &["every day"]),
        (vec![Tue, Thu], "  tue, thu", &["tue,", "thu"]),
        (vec![Sun, Sat], "", &["weekends"]),
    ];
    for (set, tail, held) in cases {
        let (mut a, a_readings) = profile("a", 95.0, 20.0, 3600);
        a.preferred_days = set;
        let (cfg, _readings) = config_with(vec![(a, a_readings)], Some("a"), vec!["a"]);
        let line = preferred_days_line(&cfg);
        assert_eq!(line_text(&line), format!("{RUN}{tail}"), "{held:?}");
        let styled: Vec<(&str, Option<ratatui::style::Color>)> = line
            .spans
            .iter()
            .skip(2)
            .filter(|s| !s.content.trim().is_empty())
            .map(|s| (s.content.as_ref(), s.style.fg))
            .collect();
        let want: Vec<(&str, Option<ratatui::style::Color>)> = styled
            .iter()
            .map(|(text, _)| {
                let fg = if held.contains(text) {
                    theme::accent().fg
                } else {
                    theme::faint().fg
                };
                (*text, fg)
            })
            .collect();
        assert_eq!(styled, want, "{held:?} is the one ACCENT value");
    }
}

/// The `└` hint under the first row carrying `key`, its wrapped lines joined
/// back into the one sentence it renders: `None` when no hint follows the row.
fn hint_after(lines: &[Line<'static>], key: &str) -> Option<String> {
    let at = lines
        .iter()
        .position(|l| line_text(l).contains(key))
        .expect("the row renders");
    let mut hint = lines[at + 1..]
        .iter()
        .map(line_text)
        .take_while(|t| t.starts_with(" └ ") || t.starts_with("   "));
    let first = hint.next()?.strip_prefix(" └ ")?.to_string();
    Some(hint.fold(first, |acc, t| format!("{acc} {}", t.trim_start())))
}

/// The row's help line, one per state, whole-sentence equality since each arm
/// names a different fact. At rest it says what the list does and that ↵ picks
/// days: blocker first (an empty list included, since the blocker is about the
/// account, not the list), then a list another member's list shares days with,
/// else the plain statement, which holds for an empty list, a set one, and one
/// sitting beside another member's list that names other days. Descended, the
/// picker's whole edit grammar.
#[test]
fn preferred_days_row_hint_names_the_blocker_then_the_list_state() {
    use chrono::Weekday::{Fri, Sat, Sun};
    let hint_under = |cfg: &AppConfig, day_picker: Option<usize>| -> Option<String> {
        let row = FALLBACK_ROWS
            .iter()
            .position(|r| *r == FallbackRow::PreferredDays)
            .expect("the row exists");
        let lines = member_detail(
            cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: row,
                day_picker,
                width: 80,
                ..Default::default()
            },
            &HashSet::new(),
            &ReadingsSnapshot::default(),
        )
        .0;
        hint_after(&lines, "preferred days")
    };
    const SAYS: &str =
        "work returns to this account on the days set here · ↵ picks days one by one";

    let (empty, _readings) =
        config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    assert_eq!(hint_under(&empty, None).as_deref(), Some(SAYS), "no list");

    let (mut set, set_readings) = profile("a", 95.0, 20.0, 3600);
    set.preferred_days = vec![Sat, Sun];
    let (set, _readings) = config_with(vec![(set, set_readings)], Some("a"), vec!["a"]);
    assert_eq!(hint_under(&set, None).as_deref(), Some(SAYS), "a list");

    // Another member's list naming other days leaves this one's claim whole.
    let (mut beside, beside_readings) = profile("a", 95.0, 20.0, 3600);
    beside.preferred_days = vec![Sat, Sun];
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.preferred_days = vec![Fri];
    let (beside, _readings) = config_with(
        vec![(beside, beside_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    assert_eq!(
        hint_under(&beside, None).as_deref(),
        Some(SAYS),
        "a list elsewhere on other days"
    );

    // A list on an account the walk skips claims nothing, so it shares no day.
    let (mut beside_dead, beside_dead_readings) = profile("a", 95.0, 20.0, 3600);
    beside_dead.preferred_days = vec![Sat, Sun];
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.disabled = true;
    b.preferred_days = vec![Fri, Sat];
    let (beside_dead, _readings) = config_with(
        vec![(beside_dead, beside_dead_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    assert_eq!(
        hint_under(&beside_dead, None).as_deref(),
        Some(SAYS),
        "a disabled member's list on a shared day"
    );

    // A day both lists name returns work to whichever claimant reads clear
    // first, so the plain statement would promise this account those days.
    let (mut shared, shared_readings) = profile("a", 95.0, 20.0, 3600);
    shared.preferred_days = vec![Sat, Sun];
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.preferred_days = vec![Fri, Sat];
    let (shared, _readings) = config_with(
        vec![(shared, shared_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    assert_eq!(
        hint_under(&shared, None).as_deref(),
        Some(
            "another list also names sat: work returns to whichever account reads clear first \
             · ↵ picks days one by one"
        ),
        "a day another list shares"
    );

    let (mut dead, dead_readings) = profile("a", 95.0, 20.0, 3600);
    dead.disabled = true;
    dead.preferred_days = vec![Sat];
    let (dead, _readings) = config_with(vec![(dead, dead_readings)], Some("other"), vec!["a"]);
    const DEAD: &str =
        "a day list here would claim nothing: the account is disabled · ↵ picks days one by one";
    assert_eq!(hint_under(&dead, None).as_deref(), Some(DEAD));

    let (mut dead_empty, dead_empty_readings) = profile("a", 95.0, 20.0, 3600);
    dead_empty.disabled = true;
    let (dead_empty, _readings) = config_with(
        vec![(dead_empty, dead_empty_readings)],
        Some("other"),
        vec!["a"],
    );
    assert_eq!(
        hint_under(&dead_empty, None).as_deref(),
        Some(DEAD),
        "the blocker leads on an empty list too"
    );
    assert_eq!(
        hint_under(&dead_empty, Some(0)).as_deref(),
        Some("← → walk · space toggles and saves · ↵ esc done · ↑ ↓ leave"),
        "descended, the picker's grammar replaces the at-rest hint"
    );
}

// The descended row is a multi-select chip row: `✎` in the gutter,
// a `[x]`/`[ ]` mark per day read off the saved list (brackets TEXT_DIM, `x`
// ACCENT), the day ACCENT picked / TEXT_FAINT unpicked, and the caret a bold
// ACCENT `❯` in a slot every chip reserves — never a background fill.
#[test]
fn the_day_picker_row_marks_picks_and_carries_its_caret_in_a_slot() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use chrono::Weekday::{Mon, Wed};
    let lines = day_picker_lines(&[Mon, Wed], 2, 80);
    assert_eq!(
        lines.iter().map(line_text).collect::<Vec<_>>(),
        vec![
            "✎ preferred days  [x]mon   [ ]tue  ❯[x]wed   [ ]thu   [ ]fri   [ ]sat   [ ]sun"
                .to_string()
        ],
        "a pane that holds the row keeps it on one line"
    );
    let spans = &lines[0].spans;
    let accent_bold = theme::accent().bold();
    assert_eq!(spans[0].content.as_ref(), "✎ ");
    assert_eq!(spans[0].style, accent_bold, "the edit glyph replaces `❯`");
    // Chip `i` is five spans (slot, `[`, mark, `]`, day) after a gap span.
    let (slot, open, mark, close, day) = (
        |i: usize| &spans[6 * i + 2],
        |i: usize| &spans[6 * i + 3],
        |i: usize| &spans[6 * i + 4],
        |i: usize| &spans[6 * i + 5],
        |i: usize| &spans[6 * i + 6],
    );
    assert_eq!(slot(2).content.as_ref(), "❯");
    assert_eq!(slot(2).style, accent_bold, "the caret is bold ACCENT");
    assert_eq!(
        slot(1).content.as_ref(),
        " ",
        "other chips keep the slot blank"
    );
    for i in 0..7 {
        assert_eq!(open(i).style, theme::dim(), "chip {i}: `[` is TEXT_DIM");
        assert_eq!(close(i).style, theme::dim(), "chip {i}: `]` is TEXT_DIM");
        assert_eq!(slot(i).style.bg, None, "chip {i}: no fill marks the caret");
        assert_eq!(day(i).style.bg, None, "chip {i}: no fill marks the caret");
    }
    assert_eq!(
        (mark(0).content.as_ref(), mark(0).style),
        ("x", theme::accent())
    );
    assert_eq!(mark(1).content.as_ref(), " ", "tue is unpicked");
    assert_eq!(day(0).style, theme::accent(), "a picked day reads ACCENT");
    assert_eq!(
        day(1).style,
        theme::faint(),
        "an unpicked day reads TEXT_FAINT"
    );
    assert_eq!(day(2).style, theme::accent());

    assert_eq!(
        day_picker_rows(1),
        (0..7).map(|i| i..i + 1).collect::<Vec<_>>(),
        "a pane too narrow for any chip still gives each day a line of its own"
    );
}

/// `+ add` names a day list the candidate carries from an earlier stint on the
/// chain, under the candidate the cursor is on, before the add re-arms it.
#[test]
fn the_add_picker_names_a_carried_day_list_before_the_add() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.preferred_days = vec![Tue, Thu];
    let (mut c, c_readings) = profile("c", 95.0, 20.0, 3600);
    c.preferred_days = vec![Sun, Sat];
    let (cfg, _readings) = config_with(
        vec![
            profile("a", 95.0, 20.0, 3600),
            (b, b_readings),
            (c, c_readings),
            profile("d", 95.0, 20.0, 3600),
        ],
        Some("a"),
        vec!["a"],
    );
    let mut app = App::new(cfg);
    // At 80 the blurb holds one line, so the candidates open on the fifth.
    let picker = |app: &App| -> Vec<String> {
        add_detail(app, true, 80, &HashSet::new())
            .lines
            .iter()
            .skip(4)
            .map(line_text)
            .map(|t| t.trim_end().to_string())
            .collect()
    };

    app.fallback_detail_cursor = 0;
    assert_eq!(
        picker(&app),
        vec![
            "❯ b",
            " └ brings back its preferred days: tue, thu",
            "  c",
            "  d"
        ],
        "a custom list is spelled out"
    );
    app.fallback_detail_cursor = 1;
    assert_eq!(
        picker(&app),
        vec![
            "  b",
            "❯ c",
            " └ brings back its preferred days: weekends",
            "  d"
        ],
        "a preset list takes the name the member row gives it"
    );
    app.fallback_detail_cursor = 2;
    assert_eq!(
        picker(&app),
        vec!["  b", "  c", "❯ d"],
        "a candidate with no list says nothing"
    );
}

/// A carried list on a candidate the walk would skip claims nothing once added,
/// so the pick names the blocker instead of promising home days.
#[test]
fn the_add_picker_names_the_blocker_before_a_carried_list() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.preferred_days = vec![chrono::Weekday::Sat, chrono::Weekday::Sun];
    let (mut cfg, _readings) = config_with(
        vec![profile("a", 95.0, 20.0, 3600), (b, b_readings)],
        Some("a"),
        vec!["a"],
    );
    cfg.state.auth_broken.push("b".into());
    let mut app = App::new(cfg);
    app.fallback_detail_cursor = 0;
    let picker: Vec<String> = add_detail(&app, true, 80, &HashSet::new())
        .lines
        .iter()
        .skip(4)
        .map(line_text)
        .map(|t| t.trim_end().to_string())
        .collect();
    assert_eq!(
        picker,
        vec![
            "❯ b",
            " └ its day list (weekends) would claim nothing: its login is auth-broken",
        ]
    );
}

/// Keys and commands inside prose on the Fallback card: the
/// auth-broken fix's command and every key the preferred-days tooltip names.
#[test]
fn chain_prose_styles_the_relogin_command_and_the_day_picker_keys() {
    use crate::testutil::{assert_prose_part, lines_buffer};
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let fix = reason_fix(&BlockedReason::AuthBroken, &ProfileName::from("a"), false);
    assert_prose_part(
        &lines_buffer(rail_hint_lines(&fix, 60, false), 60),
        "re-login with clauth login a",
        "clauth login a",
        false,
    );

    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::PreferredDays)
        .expect("the row exists");
    let card_of = |cfg: &AppConfig, day_picker: Option<usize>| {
        let lines = member_detail(
            cfg,
            &ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: row,
                day_picker,
                width: 100,
                ..Default::default()
            },
            &HashSet::new(),
            &ReadingsSnapshot::default(),
        )
        .0;
        lines_buffer(lines, 100)
    };
    let (plain, _readings) =
        config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    let card = |day_picker: Option<usize>| card_of(&plain, day_picker);
    assert_prose_part(&card(None), "· ↵ picks days one by one", "↵", true);

    let (mut dead, dead_readings) = profile("a", 95.0, 20.0, 3600);
    dead.disabled = true;
    let (dead, _readings) = config_with(vec![(dead, dead_readings)], Some("other"), vec!["a"]);
    assert_prose_part(&card_of(&dead, None), "disabled · ↵ picks days", "↵", true);

    let (mut shared, shared_readings) = profile("a", 95.0, 20.0, 3600);
    shared.preferred_days = vec![chrono::Weekday::Sat];
    let (mut b, b_readings) = profile("b", 95.0, 20.0, 3600);
    b.preferred_days = vec![chrono::Weekday::Sat];
    let (shared, _readings) = config_with(
        vec![(shared, shared_readings), (b, b_readings)],
        Some("a"),
        vec!["a", "b"],
    );
    assert_prose_part(&card_of(&shared, None), "first · ↵ picks days", "↵", true);
    let picking = card(Some(0));
    for (phrase, part) in [
        ("← → walk", "← →"),
        ("· space toggles", "space"),
        ("· ↵ esc done", "↵ esc"),
        ("· ↑ ↓ leave", "↑ ↓"),
    ] {
        assert_prose_part(&picking, phrase, part, true);
    }
}

/// A narrow card reflows the `remove` value to its own line past the caret
/// gutter once the value column leaves no room, instead of clipping the tail.
#[test]
fn the_remove_value_drops_to_its_own_line_when_narrow() {
    // The expected rows spell the full tier's toggle glyphs (`─●` / `○─`).
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let a = profile("a", 95.0, 20.0, 3600);
    let (cfg, readings) = config_with(vec![a], None, vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 24,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(
        texts,
        [
            "5h usage        ██░░░░░│",
            "                20% used",
            "                75% unt…",
            "",
            "  rotate at       95%",
            "  weekly at       98%",
            "  weekly gate     ─●",
            "  scoped gate     ─●",
            "  credit gate     ─●",
            "  last resort     ○─",
            "  preferred       ○─",
            "  preferred days  ",
            "  never  weekdays",
            "  weekends  every day",
            "  max spend       off",
            "  remove          ",
            "  remove from chain",
        ]
    );
}

/// The `+ add` heading wraps on a narrow pane instead of clipping its tail.
#[test]
fn the_add_blurb_wraps_on_a_narrow_pane() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("a", 95.0, 20.0, 3600);
    let (cfg, readings) = config_with(vec![a], None, vec!["a"]);
    let app = app_with_readings(cfg, &readings);
    let detail = add_detail(&app, true, 24, &HashSet::new());
    let texts: Vec<String> = detail.lines.iter().map(line_text).collect();
    assert_eq!(
        texts,
        [
            "add an account to the",
            "rotation",
            "",
            "when an account runs",
            "out, clauth points",
            "claude code at the next",
            "one.",
            "",
            "every account is already in the chain",
        ]
    );
}

/// The `default: 95%` reminder on a `rotate at` row drops to its own line
/// under the value column once the value column leaves no room, instead of
/// clipping the reminder mid-word.
#[test]
fn the_rotate_reminder_drops_to_its_own_line_when_narrow() {
    // The expected rows spell the full tier's toggle glyphs (`─●` / `○─`).
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let a = profile("a", 80.0, 20.0, 3600);
    let (cfg, readings) = config_with(vec![a], None, vec!["a"]);
    let lines = member_detail(
        &cfg,
        &crate::profile::ProfileName::from("a"),
        MemberCard {
            width: 30,
            ..Default::default()
        },
        &HashSet::new(),
        &readings,
    )
    .0;
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(
        texts,
        [
            "5h usage        █░░│  20% used",
            "                60% until rot…",
            "",
            "  rotate at       80%",
            "                  default: 95%",
            "  weekly at       98%",
            "  weekly gate     ─●",
            "  scoped gate     ─●",
            "  credit gate     ─●",
            "  last resort     ○─",
            "  preferred       ○─",
            "  preferred days  never",
            "                  weekdays",
            "                  weekends",
            "                  every day",
            "  max spend       off",
            "  remove          ",
            "  remove from chain",
        ]
    );
}

/// An unmarked selector name clamps to the pane edge (with `…`) instead of
/// running raw into the border — the marked arm's clamp minus the marker.
#[test]
fn an_unmarked_selector_name_clamps_to_the_pane_edge() {
    let _home = crate::testutil::HomeSandbox::new();
    let a = profile("first.last@company.example", 95.0, 20.0, 3600);
    let (cfg, readings) = config_with(vec![a], None, vec!["first.last@company.example"]);
    let app = app_with_readings(cfg, &readings);
    assert_eq!(
        chain_selector_pane(&app, 60, 8),
        vec![
            "╭─ CHAIN ──────────╮".to_string(),
            "│ ❯  #1 first.las… │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "│                  │".to_string(),
            "╰──────────────────╯".to_string(),
        ],
    );
}

fn threshold_app(draft: Option<&str>) -> (App, ReadingsSnapshot) {
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let mut app = app_with_readings(cfg, &readings);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Threshold)
        .expect("the rotate-at row is listed");
    app.fallback_edit = draft.map(|d| crate::tui::app::MemberEdit {
        member: "a".into(),
        state: CardEdit::Threshold(InputState::new(d)),
    });
    (app, readings)
}

fn threshold_draft(app: &mut App) -> &mut InputState {
    match app.fallback_edit.as_mut().map(|e| &mut e.state) {
        Some(CardEdit::Threshold(input)) => input,
        _ => panic!("the rotate-at editor is open"),
    }
}

/// S10: a threshold buffer wider than the card's value column scrolls inside
/// its row on a narrow card: End keeps the caret inside the pane right after
/// the last digit, one ← moves it one cell and leaves the row alone, Home shows
/// the first digit again.
#[test]
fn an_over_wide_threshold_buffer_scrolls_inside_its_row() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, readings) = threshold_app(Some("1234567890123456"));
    let mut term = Terminal::new(TestBackend::new(34, 40)).unwrap();

    let (end, buf) = draw_caret(&mut term, |f| {
        super::draw(f, f.area(), &app, &readings, &[])
    });
    let border = right_border(&buf, end.y);
    assert!(
        end.x + 2 <= border,
        "End stays inside the field ({end:?}, border {border})"
    );
    assert_eq!(
        symbol_at(&buf, end.x - 1, end.y),
        "6",
        "End follows the last digit"
    );
    let end_row = crate::testutil::buffer_rows(&buf)[end.y as usize].clone();

    threshold_draft(&mut app).left();
    let (left, buf) = draw_caret(&mut term, |f| {
        super::draw(f, f.area(), &app, &readings, &[])
    });
    assert_eq!(
        left,
        ratatui::layout::Position::new(end.x - 1, end.y),
        "one left moves one cell"
    );
    assert_eq!(
        crate::testutil::buffer_rows(&buf)[left.y as usize],
        end_row,
        "a move inside the window leaves the row alone"
    );

    threshold_draft(&mut app).home();
    let (home, buf) = draw_caret(&mut term, |f| {
        super::draw(f, f.area(), &app, &readings, &[])
    });
    assert_eq!(home.y, end.y, "Home stays on the field row");
    assert_eq!(
        symbol_at(&buf, home.x, home.y),
        "1",
        "Home sits on the first digit"
    );
}

/// S10 (cloudy's range rule, 2026-10-14): typing in `rotate at` swaps its
/// hint for the one-line `0-100 %` range, and the rows below move up by
/// exactly the difference (the hint takes 3 lines at 34 columns, 2 at 45 and
/// 80).
#[test]
fn the_threshold_editor_swaps_its_hint_for_its_range_while_typing() {
    let _home = crate::testutil::HomeSandbox::new();
    for (w, delta) in [(34u16, -2isize), (45, -1), (80, -1)] {
        let frame = |app: &App, readings: &ReadingsSnapshot| {
            let mut term = Terminal::new(TestBackend::new(w, 60)).unwrap();
            term.draw(|f| super::draw(f, f.area(), app, readings, &[]))
                .unwrap();
            crate::testutil::buffer_rows(term.backend().buffer())
        };
        let (resting_app, resting_readings) = threshold_app(None);
        let (editing_app, editing_readings) = threshold_app(Some("80"));
        let resting = frame(&resting_app, &resting_readings);
        let editing = frame(&editing_app, &editing_readings);
        let at = |rows: &[String], label: &str| {
            rows.iter()
                .position(|r| r.contains(label))
                .unwrap_or_else(|| panic!("[{w}] {label} renders:\n{}", rows.join("\n")))
        };
        let field = at(&resting, "rotate at");
        assert!(
            editing[field + 1].contains("└ 0-100 %"),
            "[{w}] the range tooltip sits under the edit line:\n{}",
            editing.join("\n")
        );
        crate::testutil::assert_rows_below_shift(
            &resting,
            &editing,
            field,
            at(&resting, "weekly at"),
            delta,
            4,
            &w.to_string(),
        );
    }
}

/// The Fallback card with `row`'s typed editor holding `draft` (the member
/// whose card is open: `a`).
fn member_typed_app(row: FallbackRow, draft: &str) -> (App, ReadingsSnapshot) {
    let state = match row {
        FallbackRow::Threshold => CardEdit::Threshold(InputState::new(draft)),
        FallbackRow::MaxSpend => CardEdit::MaxSpend(InputState::new(draft)),
        FallbackRow::WeeklyAt => CardEdit::Weekly(InputState::new(draft)),
        _ => panic!("the row has no typed editor"),
    };
    let (cfg, readings) = config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let mut app = app_with_readings(cfg, &readings);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == row)
        .expect("the row is listed");
    app.fallback_edit = Some(crate::tui::app::MemberEdit {
        member: "a".into(),
        state,
    });
    (app, readings)
}

/// S10: a typed Fallback-card row keeps its caret inside the pane on a narrow
/// terminal: the value column clamps and the key truncates (`…`) below
/// `MIN_VALUE_W` + the unit.
#[test]
fn a_typed_fallback_row_keeps_its_caret_inside_on_a_narrow_pane() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    let _home = crate::testutil::HomeSandbox::new();
    for (row, value, last) in [
        (FallbackRow::Threshold, "80", '0'),
        (FallbackRow::MaxSpend, "3.0", '0'),
        (FallbackRow::WeeklyAt, "95", '5'),
    ] {
        for w in [16u16, 18, 20, 22, 28, 34] {
            let (app, readings) = member_typed_app(row, value);
            let mut term = Terminal::new(TestBackend::new(w, 100)).unwrap();
            let (end, buf) = draw_caret(&mut term, |f| {
                super::draw(f, f.area(), &app, &readings, &[])
            });
            let border = right_border(&buf, end.y);
            assert!(
                end.x + 2 <= border,
                "[{row:?} {w}] End stays inside the field ({end:?}, border {border})"
            );
            assert_eq!(
                symbol_at(&buf, end.x - 1, end.y),
                last.to_string(),
                "[{row:?} {w}] End follows the last typed char"
            );
        }
    }
}

/// The P1 readings: float residue that must print as a whole, two decimals
/// that must not round to a fabricated whole, and a reading that rounds up.
const P1_READINGS: [f64; 3] = [1.000_000_000_000_000_9, 73.42, 99.999];

fn p1_card_app(threshold: f64) -> App {
    let names = ["alpha", "bravo", "charlie"];
    let profiles: Vec<(Profile, ReadingsSnapshot)> = names
        .iter()
        .zip(P1_READINGS)
        .map(|(n, r)| profile(n, threshold, r, 2 * 3600 + 30 * 60 + 30))
        .collect();
    let (cfg, readings) = config_with(profiles, Some("alpha"), names.to_vec());
    let mut app = app_with_readings(cfg, &readings);
    app.tab = crate::tui::app::Tab::Fallback;
    app
}

/// P1 verify matrix, Fallback card: the gauge figure (`… used`), the headroom
/// (`… until rotate`) and the 5h pill print each reading with at most two
/// rounded decimals. On a pane too narrow for the whole line the words give
/// way (`…`) and never the reading or its `%`.
#[test]
fn the_member_card_keeps_two_rounded_decimals_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = p1_card_app(80.0);
    let want: &[(usize, u16, &[&str])] = &[
        (
            0,
            28,
            &[
                "│                 1% used  │",
                "│                 79% unt… │",
            ],
        ),
        (
            1,
            28,
            &[
                "│                 73.42% … │",
                "│                 6.58% u… │",
            ],
        ),
        (
            2,
            28,
            &[
                "│   [ 5h 100% ]  2h 30m   ┃│",
                "│                 100% us…┃│",
                "│                 0% unti…┃│",
            ],
        ),
        (
            0,
            45,
            &[
                "│ 5h usage        ░░░░░░░░░░░░░│░░  1% used │",
                "│                 79% until rotate          │",
            ],
        ),
        (
            1,
            45,
            &[
                "│ 5h usage        █████████░│░  73.42% used │",
                "│                 6.58% until rotate        │",
            ],
        ),
        (
            2,
            45,
            &[
                "│ status          [ 5h 100% ]  2h 30m       │",
                "│ 5h usage        ███████████│██  100% used │",
                "│                 0% until rotate           │",
            ],
        ),
        (
            0,
            80,
            &[
                "│ ❯  #1 alpha          ││ 5h usage        ░░░░░░░░░░░░░░░░░░│░░░  1% used      │",
                "│    #2 bravo          ││                 79% until rotate                     │",
            ],
        ),
        (
            1,
            80,
            &[
                "│    #1 alpha          ││ 5h usage        ████████████████░░│░░░  73.42% used  │",
                "│ ❯  #2 bravo          ││                 6.58% until rotate                   │",
            ],
        ),
        (
            2,
            80,
            &[
                "│    #1 alpha          ││ status          [ 5h 100% ]  2h 30m                  │",
                "│                      ││ 5h usage        ██████████████████│███  100% used    │",
                "│                      ││                 0% until rotate                      │",
            ],
        ),
    ];
    for (cursor, w, rows) in want {
        app.chain_cursor = *cursor;
        let got: Vec<String> = render_chain(&app, *w, 30)
            .into_iter()
            .filter(|r| r.contains('%') && !r.contains("rotate at") && !r.contains("weekly at"))
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(
            got, *rows,
            "card reading {} at {w} cols",
            P1_READINGS[*cursor]
        );
    }
}

/// P1 verify matrix, Fallback pills: every member past a 1% line is blocked,
/// so each reading quotes in its `5h` pill. Where the value column cannot hold
/// the pill it drops under the `status` key, whole with its `%`.
#[test]
fn the_five_hour_pill_keeps_two_rounded_decimals_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = p1_card_app(1.0);
    let want: &[(usize, u16, &str)] = &[
        (0, 28, "│   [ 5h 1% ]  2h 30m     ┃│"),
        (1, 28, "│   [ 5h 73.42% ]  2h 30m ┃│"),
        (2, 28, "│   [ 5h 100% ]  2h 30m   ┃│"),
        (0, 45, "│ status          [ 5h 1% ]  2h 30m         │"),
        (1, 45, "│ status          [ 5h 73.42% ]  2h 30m     │"),
        (2, 45, "│ status          [ 5h 100% ]  2h 30m       │"),
        (
            0,
            80,
            "│ ❯  #1 alpha        ◔ ││ status          [ 5h 1% ]  2h 30m                    │",
        ),
        (
            1,
            80,
            "│    #1 alpha        ◔ ││ status          [ 5h 73.42% ]  2h 30m                │",
        ),
        (
            2,
            80,
            "│    #1 alpha        ◔ ││ status          [ 5h 100% ]  2h 30m                  │",
        ),
    ];
    for (cursor, w, row) in want {
        app.chain_cursor = *cursor;
        let rows: Vec<String> = render_chain(&app, *w, 30)
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        // P1/R3: the pill sits under its `status` key when it drops.
        let pill = rows
            .iter()
            .position(|r| r.contains("[ 5h"))
            .expect("pill renders");
        let before = rows[pill.saturating_sub(1)].as_str();
        if *w == 28 {
            assert!(
                before.starts_with("│ status"),
                "the dropped pill's status key row: {before:?}"
            );
        } else {
            assert!(
                rows[pill].contains("│ status"),
                "the pill rides the status key row: {:?}",
                rows[pill]
            );
        }
        assert_eq!(
            rows[pill], *row,
            "pill reading {} at {w} cols",
            P1_READINGS[*cursor]
        );
    }
}

/// A setting is not a reading: beside a 73.42% reading the card's `rotate at`
/// and `weekly at` rows keep their whole-number spelling, at every width.
#[test]
fn thresholds_keep_their_own_spelling_beside_a_decimal_reading() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = p1_card_app(80.0);
    app.chain_cursor = 1;
    let want: &[(u16, [&str; 2])] = &[
        (
            28,
            [
                "│   rotate at       80%    │",
                "│   weekly at       98%    │",
            ],
        ),
        (
            45,
            [
                "│   rotate at       80%   default: 95%      │",
                "│   weekly at       98%                     │",
            ],
        ),
        (
            80,
            [
                "│                      ││   rotate at       80%   default: 95%                 │",
                "│                      ││   weekly at       98%                                │",
            ],
        ),
    ];
    for (w, rows) in want {
        let got: Vec<String> = render_chain(&app, *w, 30)
            .into_iter()
            .filter(|r| r.contains("rotate at") || r.contains("weekly at"))
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(got, *rows, "threshold rows beside 73.42% at {w} cols");
    }
}

/// A `ScopedSpent` pill whose server-named label carries the reading: the
/// fixture the P1 narrow-width pins use.
fn p1_scoped_app(label: &str) -> App {
    let (mut p, mut readings) = profile("alpha", 95.0, 20.0, 9030);
    p.weekly_threshold = Some(50.0);
    if let Some(u) = readings.usage.get_mut("alpha") {
        u.seven_day = Some(UsageWindow {
            utilization: 10.0,
            resets_at: Some(reset_in(5 * 86_400)),
        });
        u.weekly_scoped = vec![crate::usage::ScopedWindow {
            label: label.to_string(),
            window: UsageWindow {
                utilization: 73.42,
                resets_at: Some(reset_in(5 * 86_400)),
            },
        }];
    }
    let (cfg, readings) = config_with(vec![(p, readings)], Some("alpha"), vec!["alpha"]);
    let mut app = app_with_readings(cfg, &readings);
    app.tab = crate::tui::app::Tab::Fallback;
    app
}

/// P1/R2: a dropped pill too wide for its line cuts its label's name with the
/// shared truncate — the reading and the closing ` ]` stay whole, at 28 and 29.
#[test]
fn a_dropped_scoped_pill_cuts_its_name_never_the_reading() {
    let _home = crate::testutil::HomeSandbox::new();
    let want: &[(&str, u16, &str)] = &[
        ("7d sonnet 4.5", 28, "│   [ 7d sonnet … 73.42% ]┃│"),
        ("7d opus 4.1 fast", 28, "│   [ 7d opus 4.… 73.42% ]┃│"),
        ("7d sonnet 4.5", 29, "│   [ 7d sonnet 4… 73.42% ]┃│"),
        ("7d opus 4.1 fast", 29, "│   [ 7d opus 4.1… 73.42% ]┃│"),
    ];
    for (label, w, row) in want {
        let app = p1_scoped_app(label);
        let rows: Vec<String> = render_chain(&app, *w, 30)
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        let pill = rows
            .iter()
            .position(|r| r.contains("73.42%"))
            .expect("pill renders");
        // P1/R3: the dropped pill's `status` key row sits right above it.
        assert!(
            rows[pill - 1].starts_with("│ status"),
            "the status key row above the dropped pill for {label:?} at {w}: {:?}",
            rows[pill - 1]
        );
        assert_eq!(rows[pill], *row, "{label:?} at {w} cols");
    }
}

/// P1/R4: a later pill dropped under the first keeps the `│ `-led line and no
/// empty rail row appears between them.
#[test]
fn a_later_dropped_pill_gets_no_empty_rail_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut d, d_readings) = profile("bravo", 50.0, 73.42, 3600);
    d.disabled = true;
    let (cfg, readings) = config_with(
        vec![profile("alpha", 95.0, 10.0, 3600), (d, d_readings)],
        Some("alpha"),
        vec!["alpha", "bravo"],
    );
    let mut app = app_with_readings(cfg, &readings);
    app.tab = crate::tui::app::Tab::Fallback;
    app.chain_cursor = 1;
    let rows: Vec<String> = render_chain(&app, 28, 30)
        .into_iter()
        .map(|r| r.trim_end().to_string())
        .collect();
    let health = rows
        .iter()
        .position(|r| r.contains("73.42%"))
        .expect("the health pill renders");
    // The bridge rides the dropped pill's own line: the rows above it are the
    // first pill's hint rail, never an empty rail row.
    assert_eq!(
        &rows[health - 1..=health],
        &[
            "│ │ setup tab             ┃│",
            "│ │ [ 5h 73.42% ]  1h 0m  ┃│",
        ],
        "the later pill bridges the rail on its own line"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("[ disabled ]")).count(),
        1,
        "the first pill stays on the key row"
    );
}

/// A Fallback member field's caret that lands inside a drawn toast hides the
/// native cursor this frame. Three stacked DANGER toasts cover the `rotate at`
/// field's caret column at 45x14.
#[test]
fn a_toast_over_a_fallback_field_caret_hides_the_native_cursor() {
    use crate::tui::app::{Tab, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    let (mut app, _readings) = member_typed_app(FallbackRow::Threshold, "80");
    app.tab = Tab::Fallback;

    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the fallback caret shows with no toasts"
    );

    let wide = "x".repeat(53);
    for name in ["one", "two", "three"] {
        app.toast(ToastKind::Danger, format!("{name}\n{wide}\n{name} tail"));
    }

    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "[45x14] the fallback caret hides under a toast"
    );
}

/// While a modal is open, a body editor under it must not leave its caret
/// visible on the modal (it would blink on the modal's border); a modal text
/// field still places its own caret. The Fallback `rotate at` field stays open
/// under the loss warning, and its caret would land on the modal's border row.
#[test]
fn a_body_caret_hides_under_an_open_modal_but_a_modal_field_keeps_its_caret() {
    use crate::actions::{AnchorAction, CaptureSnapshot};
    use crate::tui::app::{
        CaptureNameForm, InputState, LossWarningDrop, LossWarningState, Modal, Tab,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    // A body editor with the loss warning open: no caret on the modal border.
    let (mut body, _readings) = member_typed_app(FallbackRow::Threshold, "80");
    body.tab = Tab::Fallback;
    body.modals.push(Modal::LossWarning(LossWarningState {
        choice: false,
        target: Tab::Setup,
        drop: LossWarningDrop::FallbackEdit,
    }));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &body)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "a body caret hides under an open modal"
    );

    // A modal text field still places its caret (the blocker must not reach
    // the modal's own draw).
    let mut app = crate::tui::app::App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Overview;
    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(CaptureSnapshot {
            credentials: None,
            base_url: Some("https://new.example.com".to_string()),
            api_key: Some("new-key".to_string()),
            anchor: AnchorAction::Unproven,
            plan: None,
        }),
        input: InputState::new("a name typed into the field"),
        from_divergence: false,
    }));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "a modal field keeps its caret under the blocker"
    );
}

// The credit gate row (#93): a toggle whose hint states the walk's current
// behaviour for this account, in the approved copy (cloudy, 2026-10-19).
#[test]
fn the_credit_gate_row_hints_the_current_state() {
    let row = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::CheckCreditCap)
        .expect("the credit gate is a Fallback row");
    assert_eq!(
        FALLBACK_ROWS[row - 1],
        FallbackRow::CheckScoped,
        "it sits right after the scoped gate"
    );
    let hint = |check_credit_cap: bool| {
        let (mut p, p_readings) = profile("a", 95.0, 20.0, 3600);
        p.check_credit_cap = check_credit_cap;
        let (cfg, readings) = config_with(vec![(p, p_readings)], Some("a"), vec!["a"]);
        let lines = member_detail(
            &cfg,
            &crate::profile::ProfileName::from("a"),
            MemberCard {
                focused: true,
                row_cursor: row,
                width: 120,
                ..Default::default()
            },
            &HashSet::new(),
            &readings,
        )
        .0;
        hint_after(&lines, "credit gate")
    };
    assert_eq!(
        hint(true).as_deref(),
        Some(
            "a spent usage-credit cap keeps auto-switch off this account, except when marked last resort or when recovering from a switch-off"
        )
    );
    assert_eq!(
        hint(false).as_deref(),
        Some("auto-switch can move onto this account even with its usage-credit cap spent")
    );
}
