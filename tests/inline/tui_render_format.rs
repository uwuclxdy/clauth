//! `fixed_split` truncation contract: content + pad always total `width`, and
//! ANY dropped character is signalled with a trailing `…` — including the
//! boundary case where the value is exactly one char over the window, which
//! the old `for`-loop consumed and mistook for end-of-string.

use super::*;

fn joined(value: &str, width: usize) -> String {
    let (content, pad) = fixed_split(value, width);
    format!("{content}{pad}")
}

#[test]
fn fits_exactly_no_ellipsis() {
    assert_eq!(fixed_split("Max 20", 6), ("Max 20".into(), "".into()));
}

#[test]
fn shorter_pads_to_width() {
    assert_eq!(fixed_split("ok", 5), ("ok".into(), "   ".into()));
}

/// The off-by-one: one char over the window must still truncate visibly.
#[test]
fn one_char_over_truncates_with_ellipsis() {
    assert_eq!(fixed_split("Max 20x", 6).0, "Max 2…");
    assert_eq!(fixed_split("x@computelabs.ai", 15).0, "x@computelabs.…");
}

#[test]
fn far_over_truncates_with_ellipsis() {
    assert_eq!(fixed_split("a-long-account-name", 8).0, "a-long-…");
}

#[test]
fn width_zero_yields_nothing() {
    assert_eq!(fixed_split("anything", 0), (String::new(), String::new()));
}

/// Invariant across the boundary: rendered cell is always exactly `width`
/// chars for any non-empty value.
#[test]
fn cell_is_always_exactly_width() {
    for len in 0..12usize {
        let value: String = "abcdefghijkl".chars().take(len).collect();
        for width in 1..10usize {
            assert_eq!(
                joined(&value, width).chars().count(),
                width,
                "value len {len}, width {width}"
            );
        }
    }
}

// ── fetch-state cue ──────────────────────────────────────────────────────
//
// `fetch_cue_color`: amber = serving last-known numbers, red = failed, none =
// live. The overview countdown carries this cue; brackets stay plain dim.

fn cue_profile(status: Option<FetchStatus>) -> Profile {
    Profile {
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: "p".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: std::collections::BTreeMap::new(),
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
        fetch_status: status,
        provider: None,
        third_party_usage: None,
    }
}

fn cue_window(util: f64) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: None,
    }
}

#[test]
fn cue_amber_on_cached_and_rate_limited() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::Cached))),
        Some(theme::warning_color())
    );
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::RateLimited))),
        Some(theme::warning_color())
    );
}

#[test]
fn cue_red_on_failed() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::Failed))),
        Some(theme::danger_color())
    );
}

#[test]
fn cue_absent_when_live_or_never_fetched() {
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::Fresh))),
        None
    );
    assert_eq!(fetch_cue_color(&cue_profile(None)), None);
}

/// API-key/provider rows have no oauth fetch leg — a stray status must not
/// paint their brackets.
#[test]
fn cue_absent_for_api_key_profiles() {
    let mut p = cue_profile(Some(FetchStatus::Failed));
    p.base_url = Some("https://api.example.com".into());
    assert_eq!(fetch_cue_color(&p), None);
}

#[test]
fn brackets_stay_dim_regardless_of_fetch_state() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let w = cue_window(50.0);
    let spans =
        window_summary_spans_bracketed(Some(&w), 17, true, None, ResetFmt::default(), false);
    assert_eq!(spans[0].content, "[");
    assert_eq!(spans[0].style.fg, theme::dim().fg);
    assert_eq!(spans[2].content, "]");
    assert_eq!(spans[2].style.fg, theme::dim().fg);
}

/// A failed fetch with nothing cached renders only `—`, styled faint like any
/// other no-data cell — the cue lives on the overview countdown instead.
#[test]
fn no_data_dash_stays_faint() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let spans = window_summary_spans_bracketed(None, 17, true, None, ResetFmt::default(), false);
    assert_eq!(spans[0].content, "—");
    assert_eq!(spans[0].style.fg, theme::faint().fg);
}

// ── Reset display (issue #39) ───────────────────────────────────────────────
//
// `clock_text` and the three compositions are pure, so these pin exact strings
// without depending on the box's timezone. The zone lookup itself
// (`local_clock`) is a single chrono call over them.

use chrono::NaiveDate;

fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .expect("valid date")
        .and_hms_opt(h, min, 0)
        .expect("valid time")
}

#[test]
fn clock_text_drops_the_day_on_todays_date() {
    let now = at(2026, 6, 21, 8, 5);
    assert_eq!(
        clock_text(at(2026, 6, 21, 21, 20), now, ClockFormat::H24, Day::Qualify),
        "21:20"
    );
    assert_eq!(
        clock_text(at(2026, 6, 21, 21, 20), now, ClockFormat::H12, Day::Qualify),
        "9:20pm"
    );
}

/// 12-hour notation has no zero hour and no 0 o'clock: midnight and noon both
/// read `12`, and the meridiem is what separates them.
#[test]
fn clock_text_h12_handles_midnight_and_noon() {
    let now = at(2026, 6, 21, 8, 5);
    assert_eq!(
        clock_text(at(2026, 6, 21, 0, 5), now, ClockFormat::H12, Day::Qualify),
        "12:05am"
    );
    assert_eq!(
        clock_text(at(2026, 6, 21, 12, 5), now, ClockFormat::H12, Day::Qualify),
        "12:05pm"
    );
    // The same two instants stay unambiguous in 24-hour, zero-padded.
    assert_eq!(
        clock_text(at(2026, 6, 21, 0, 5), now, ClockFormat::H24, Day::Qualify),
        "00:05"
    );
    assert_eq!(
        clock_text(at(2026, 6, 21, 12, 5), now, ClockFormat::H24, Day::Qualify),
        "12:05"
    );
}

/// The day qualifier is what makes a 7d window's reset readable. A weekday
/// alone only works while it can't wrap onto today's own name, so day 7 and
/// beyond takes the date instead.
#[test]
fn clock_text_qualifies_the_day_by_distance() {
    let now = at(2026, 6, 21, 8, 5); // a sunday
    assert_eq!(
        clock_text(at(2026, 6, 22, 21, 20), now, ClockFormat::H24, Day::Qualify),
        "mon 21:20"
    );
    assert_eq!(
        clock_text(at(2026, 6, 27, 21, 20), now, ClockFormat::H24, Day::Qualify),
        "sat 21:20"
    );
    assert_eq!(
        clock_text(at(2026, 6, 28, 21, 20), now, ClockFormat::H24, Day::Qualify),
        "jun 28, 21:20"
    );
    // An instant already past never borrows a weekday it would share with today.
    assert_eq!(
        clock_text(at(2026, 6, 19, 21, 20), now, ClockFormat::H24, Day::Qualify),
        "jun 19, 21:20"
    );
}

/// The countdown never disappears: an unresolvable stamp (or the setting off)
/// falls back to it on every surface, in every mode.
#[test]
fn every_surface_falls_back_to_the_countdown_without_a_clock() {
    for display in [
        ResetDisplay::Relative,
        ResetDisplay::Clock,
        ResetDisplay::Both,
    ] {
        assert_eq!(phrase_text("40m", None, display), "resets in 40m");
        assert_eq!(pill_text("40m", None, display), "40m");
        assert_eq!(column_text("40m", None, display), "40m");
    }
}

#[test]
fn each_surface_words_the_clock_its_own_way() {
    let at = Some("21:20");
    assert_eq!(
        phrase_text("40m", at, ResetDisplay::Clock),
        "resets at 21:20"
    );
    assert_eq!(
        phrase_text("40m", at, ResetDisplay::Both),
        "resets in 40m (21:20)"
    );
    // The pill's `until` keeps a bare stamp from reading as when the block began.
    assert_eq!(pill_text("40m", at, ResetDisplay::Clock), "until 21:20");
    assert_eq!(pill_text("40m", at, ResetDisplay::Both), "40m (21:20)");
    // The overview wraps its own parens, so `both` joins on `·`, not a nested pair.
    assert_eq!(column_text("40m", at, ResetDisplay::Clock), "21:20");
    assert_eq!(column_text("40m", at, ResetDisplay::Both), "40m · 21:20");
}

/// The stock relative form is never fit-checked, so the overview column renders
/// exactly what it did before the setting existed — including the countdowns
/// that already ran a cell over the 26-wide column.
#[test]
fn relative_reset_suffix_ignores_the_column_width() {
    let fmt = ResetFmt::default();
    for width in [0, 17, 26, 120] {
        assert_eq!(reset_suffix(2400, fmt, width), " (40m)");
    }
    assert_eq!(reset_suffix(6 * 86400 + 82800, fmt, 26), " (6d 23h)");
}

/// A stamp that doesn't fit the column degrades to the bare countdown rather
/// than overflowing the row.
#[test]
fn clock_reset_suffix_degrades_when_the_column_is_too_narrow() {
    let fmt = ResetFmt {
        display: ResetDisplay::Both,
        clock: ClockFormat::H24,
    };
    let roomy = reset_suffix(2400, fmt, 120);
    assert!(
        roomy.starts_with(" (40m · ") && roomy.ends_with(')'),
        "a wide column carries both halves, got {roomy}"
    );
    assert_eq!(
        reset_suffix(2400, fmt, 26),
        " (40m)",
        "26 cells can't fit a stamp: fall back to the countdown"
    );
}

/// An overdue reset must not be dressed up as a future time. `/usage` data
/// outlives its window whenever a fetch fails, and `resets at 17:42` at 19:42 is
/// a wrong claim rather than a cosmetic one — every surface drops back to the
/// countdown, which already reads `now` there.
#[test]
fn an_overdue_reset_never_renders_a_clock() {
    for display in [ResetDisplay::Clock, ResetDisplay::Both] {
        let fmt = ResetFmt {
            display,
            clock: ClockFormat::H24,
        };
        for overdue in [0, -60, -3 * 86400] {
            assert_eq!(
                reset_phrase(overdue, fmt),
                "resets in now",
                "{display:?} at {overdue}s must not promise a past instant"
            );
            assert_eq!(reset_pill(overdue, fmt), "now");
            assert_eq!(reset_column(overdue, fmt), "now");
            assert_eq!(reset_resume(overdue, fmt), "in ~now");
        }
    }
}

/// The all-exhausted caption reads `resumes: kerry <tail>`. Its `~` rides the
/// countdown alone — the stamp comes from a stored `resets_at`, not an estimate.
#[test]
fn the_resume_caption_keeps_its_tilde_on_the_countdown() {
    let at = Some("21:20");
    assert_eq!(resume_text("4h 0m", None, ResetDisplay::Both), "in ~4h 0m");
    assert_eq!(resume_text("4h 0m", at, ResetDisplay::Clock), "at 21:20");
    assert_eq!(
        resume_text("4h 0m", at, ResetDisplay::Both),
        "in ~4h 0m (21:20)"
    );
}

/// The overview column is the tightest surface on screen, so the stamp has to
/// fit the WORST product of a countdown and a clock, not the friendliest one:
/// a 7d window resetting days out, in 12-hour notation, past midnight. Pins the
/// budget `OverviewWidths` sizes its wide tier from.
#[test]
fn the_widest_column_text_still_fits_the_wide_overview_tier() {
    // `36` wide tier − 17 bar block − 3 for the wrapping ` (…)`.
    const BUDGET: usize = 36 - 17 - 3;
    let worst_both = column_text("6d 23h", Some("12:05am"), ResetDisplay::Both);
    assert_eq!(worst_both, "6d 23h · 12:05am");
    assert!(
        worst_both.chars().count() <= BUDGET,
        "the widest `both` column must fit: {worst_both}"
    );
    // Clock-only has no countdown to carry the day, so it keeps the qualifier —
    // and the date form is the widest thing it can produce.
    let worst_clock = column_text("6d 23h", Some("jul 26, 12:05am"), ResetDisplay::Clock);
    assert!(
        worst_clock.chars().count() <= BUDGET,
        "the widest `clock` column must fit: {worst_clock}"
    );
}

/// The `·` joining a countdown to its stamp in the overview column stays neutral
/// (inherits the paragraph's base style). A drain-colored reset then reads as a
/// styled run broken by a plain separator instead of a solid colored bar, and
/// the drain hue stays on the numbers it signals. Only `both` mode produces one.
#[test]
fn overview_reset_middle_dot_stays_uncolored_in_both_mode() {
    use ratatui::style::Color;
    let w = UsageWindow {
        utilization: 40.0,
        resets_at: Some(crate::usage::epoch_secs_to_iso(
            crate::usage::now_epoch_secs() + 40 * 60,
        )),
    };
    let fmt = ResetFmt {
        display: ResetDisplay::Both,
        clock: ClockFormat::H24,
    };
    let drain = Color::Yellow;
    let spans = window_summary_spans_bracketed(
        Some(&w),
        160,
        true,
        Some(Style::default().fg(drain)),
        fmt,
        false,
    );
    let dot = spans
        .iter()
        .find(|s| s.content == " · ")
        .expect("`both` joins the countdown and stamp on a raw separator");
    assert!(
        dot.style.fg.is_none(),
        "the middle dot carries no color, got {:?}",
        dot.style.fg
    );
    assert!(
        spans
            .iter()
            .find(|s| s.content.contains("40m"))
            .is_some_and(|s| s.style.fg == Some(drain)),
        "the countdown keeps the drain color"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.style.fg == Some(drain) && s.content.contains(':')),
        "the stamp keeps the drain color"
    );

    // No other mode emits a separator span, so the suffix lands as one span.
    for no_dot in [
        ResetFmt {
            display: ResetDisplay::Relative,
            clock: ClockFormat::H24,
        },
        ResetFmt {
            display: ResetDisplay::Clock,
            clock: ClockFormat::H24,
        },
    ] {
        let single = window_summary_spans_bracketed(Some(&w), 160, true, None, no_dot, false);
        assert!(
            !single.iter().any(|s| s.content == " · "),
            "no separator without a countdown+stamp pair"
        );
    }
}

// ── stale (past-reset) predicate ────────────────────────────────────────
//
// `is_past_reset`: true once the window's stored reset instant has passed,
// so its utilization is a frozen pre-reset reading awaiting the next fetch.

fn reset_window(offset_secs: i64) -> UsageWindow {
    UsageWindow {
        utilization: 50.0,
        resets_at: Some(crate::usage::epoch_secs_to_iso(
            crate::usage::now_epoch_secs() + offset_secs,
        )),
    }
}

#[test]
fn is_past_reset_true_once_the_stamp_has_passed() {
    assert!(is_past_reset(&reset_window(-60)));
}

#[test]
fn is_past_reset_false_while_the_stamp_is_future() {
    assert!(!is_past_reset(&reset_window(60)));
}

#[test]
fn is_past_reset_false_with_no_reset_stamp() {
    let w = UsageWindow {
        utilization: 50.0,
        resets_at: None,
    };
    assert!(!is_past_reset(&w));
}

/// The exact boundary: a reset instant equal to `now` counts as past
/// (`secs <= 0`), not future.
#[test]
fn is_past_reset_true_at_the_exact_boundary() {
    assert!(is_past_reset(&reset_window(0)));
}

/// `stale` fades only the fill + `%` spans to `theme::faint()`; the brackets
/// and the reset suffix keep their own styling either way. Fixture holds real
/// (non-zero) fill so a color swap is observable — a mutation dropping the
/// fade must red this.
#[test]
fn stale_window_fades_only_fill_and_percent() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let w = reset_window(-60);
    // A non-faint reset_style so the "reset suffix unchanged" assertion below can
    // actually red: with `None` the suffix defaults to faint on both calls, hiding
    // a mutation that leaked `stale` into the reset color.
    let reset = Some(theme::warning());
    let live =
        window_summary_spans_bracketed(Some(&w), 26, true, reset, ResetFmt::default(), false);
    let stale =
        window_summary_spans_bracketed(Some(&w), 26, true, reset, ResetFmt::default(), true);

    // spans[0] = `[`, [1] = fill, [2] = `]`, [3] = ` XX%`, [4..] = reset suffix.
    assert_eq!(live[1].content, "█████░░░░░");
    assert_ne!(
        live[1].style.fg,
        theme::faint().fg,
        "the live fixture must carry a real util color, not already-faint"
    );
    assert_eq!(stale[1].content, live[1].content, "fill glyphs unchanged");
    assert_eq!(stale[1].style.fg, theme::faint().fg, "fill fades stale");
    assert_eq!(stale[3].style.fg, theme::faint().fg, "% fades stale");

    // Brackets and the reset suffix are untouched by staleness.
    assert_eq!(stale[0].style.fg, live[0].style.fg, "`[` unchanged");
    assert_eq!(stale[2].style.fg, live[2].style.fg, "`]` unchanged");
    assert_eq!(
        stale[4..].iter().map(|s| s.style.fg).collect::<Vec<_>>(),
        live[4..].iter().map(|s| s.style.fg).collect::<Vec<_>>(),
        "reset suffix unchanged"
    );
}

// ── Fork-only tests (codex engine, RESCUE, CLA-FEED, forecast, email column) ──

// CDX-1 T8: the kind column is the harness tag for codex profiles.
#[test]
fn account_type_label_tags_codex_profiles() {
    let mut p = crate::testutil::blank_profile("cdx");
    p.harness = crate::profile::Harness::Codex;
    assert_eq!(account_type_label(&p), "Codex");
}

// ── Reset display (issue #39) ───────────────────────────────────────────────
//
// `clock_text` and the three compositions are pure, so these pin exact strings
// without depending on the box's timezone. The zone lookup itself
// (`local_clock`) is a single chrono call over them.
