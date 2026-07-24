use super::*;

// --- gap_boundary ---

#[test]
fn gap_boundary_returns_0_when_no_gap() {
    let entries = vec![(1000, 10.0), (2000, 20.0), (3000, 30.0)];
    assert_eq!(gap_boundary(&entries, 5000), 0);
}

#[test]
fn gap_boundary_returns_0_when_util_changed() {
    let entries = vec![(1000, 10.0), (3_700_000, 20.0)];
    assert_eq!(gap_boundary(&entries, 3_600_000), 0);
}

#[test]
fn gap_boundary_returns_0_when_gap_too_small() {
    let entries = vec![(1000, 10.0), (2000, 10.0)];
    assert_eq!(gap_boundary(&entries, 5000), 0);
}

#[test]
fn gap_boundary_cuts_at_idle_gap() {
    let entries = vec![
        (1000, 30.0),
        (3_700_000, 30.0), // ~1h later, same util
    ];
    assert_eq!(gap_boundary(&entries, 600_000), 1);
}

#[test]
fn gap_boundary_finds_most_recent_gap() {
    let entries = vec![
        (1000, 30.0),
        (3_700_000, 30.0), // gap 1: ~1h, same util
        (4_000_000, 31.0),
        (8_000_000, 31.0), // gap 2: ~1.1h, same util
    ];
    assert_eq!(gap_boundary(&entries, 600_000), 3);
}

#[test]
fn gap_boundary_clock_step_backwards_no_panic() {
    let now = 1_200_000_000u64;
    let entries = vec![
        (now, 30.0),
        (now - 3_600_000, 30.0), // 1h in the past (clock rewind)
    ];
    // saturating_sub gives 0, not > max_gap_ms
    assert_eq!(gap_boundary(&entries, 600_000), 0);
}

// --- compute_burn_rates_from_history ---
// Timestamps are relative to `crate::usage::now_ms()` so the real clock
// inside the function produces plausible dt values.

fn make_win(util: f64) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: None,
    }
}

fn make_info(five_h: Option<f64>, seven_d: Option<f64>) -> UsageInfo {
    UsageInfo {
        plan: None,
        five_hour: five_h.map(make_win),
        seven_day: seven_d.map(make_win),
        weekly_scoped: Vec::new(),
        window_dollars: Vec::new(),
        extra_usage: None,
        spend: None,
        codex_rate_limit_reached: None,
    }
}

// 5h windows: lookback 1h, min 3 distinct samples, gap-cut 10 min.
const FIVE_H_LOOKBACK: u64 = 60 * 60 * 1000;
const SEVEN_D_LOOKBACK: u64 = 24 * 60 * 60 * 1000;
const MIN_SAMPLES: usize = 3;
const GAP_CUT: u64 = 10 * 60 * 1000;

#[test]
fn steady_linear_drain_exact_rate() {
    let now = crate::usage::now_ms();
    // Perfectly linear climb: a weighted fit recovers the exact slope
    // regardless of the recency weighting. 10→18 over 8 min, current 20.
    let history = vec![
        ((now - 600_000), make_info(Some(10.0), None)),
        ((now - 480_000), make_info(Some(12.0), None)),
        ((now - 360_000), make_info(Some(14.0), None)),
        ((now - 240_000), make_info(Some(16.0), None)),
        ((now - 120_000), make_info(Some(18.0), None)),
    ];
    let five_h = make_win(20.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("5h", &five_h)],
        FIVE_H_LOOKBACK,
        MIN_SAMPLES,
        GAP_CUT,
    );
    let rate = rates.get("5h").copied().flatten().unwrap();
    // +2%/120s = 60 %/h on a straight line.
    assert!((rate - 60.0).abs() < 0.5, "rate={rate}");
}

#[test]
fn idle_gap_cut_yields_burst_rate() {
    let now = crate::usage::now_ms();
    // Flat at 30% for ~1h (idle), then a burst 30→32.5 over 3 min, current 33.
    // Gap-cut + dedup drop the idle plateau so the rate reflects the burst.
    let history = vec![
        ((now - 3_900_000), make_info(Some(30.0), None)), // pre-idle
        ((now - 300_000), make_info(Some(30.0), None)),   // bridge
        ((now - 240_000), make_info(Some(31.0), None)),
        ((now - 180_000), make_info(Some(32.0), None)),
        ((now - 120_000), make_info(Some(32.5), None)),
    ];
    let five_h = make_win(33.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("5h", &five_h)],
        FIVE_H_LOOKBACK,
        MIN_SAMPLES,
        GAP_CUT,
    );
    let rate = rates.get("5h").copied().flatten().unwrap();
    // Weighted fit over the 5 burst samples ≈ 35 %/h.
    assert!((rate - 35.0).abs() < 3.0, "rate={rate}");
}

#[test]
fn too_few_samples_yields_none() {
    let now = crate::usage::now_ms();
    // Only 2 distinct samples in the lookback (history point + current) — below
    // MIN_SAMPLES, so no rate is shown.
    let history = vec![((now - 120_000), make_info(Some(18.0), None))];
    let five_h = make_win(20.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("5h", &five_h)],
        FIVE_H_LOOKBACK,
        MIN_SAMPLES,
        GAP_CUT,
    );
    assert!(rates.get("5h").copied().flatten().is_none());
}

#[test]
fn lookback_excludes_pre_cutoff_samples() {
    let now = crate::usage::now_ms();
    // Two samples sit beyond the 24h cap; only `current` is inside it. After the
    // cap that leaves a single sample (< MIN_SAMPLES) → None. Proves old samples
    // are dropped by the hard lookback, not merely down-weighted.
    let history = vec![
        ((now - 40 * 3_600_000), make_info(None, Some(5.0))),
        ((now - 30 * 3_600_000), make_info(None, Some(8.0))),
    ];
    let seven_d = make_win(11.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("7d", &seven_d)],
        SEVEN_D_LOOKBACK,
        MIN_SAMPLES,
        0,
    );
    assert!(rates.get("7d").copied().flatten().is_none());
}

#[test]
fn seven_day_rate_from_capped_window() {
    let now = crate::usage::now_ms();
    // ~70h of history; the 24h lookback keeps only the last 4 samples. 7d
    // windows are sluggish by design, so the per-hour slope stays small.
    let history = vec![
        ((now - 70 * 3_600_000), make_info(None, Some(5.0))),
        ((now - 58 * 3_600_000), make_info(None, Some(8.0))),
        ((now - 46 * 3_600_000), make_info(None, Some(11.0))),
        ((now - 34 * 3_600_000), make_info(None, Some(14.0))),
        ((now - 22 * 3_600_000), make_info(None, Some(17.0))),
        ((now - 10 * 3_600_000), make_info(None, Some(19.0))),
        ((now - 4 * 3_600_000), make_info(None, Some(20.0))),
    ];
    let seven_d = make_win(21.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("7d", &seven_d)],
        SEVEN_D_LOOKBACK,
        MIN_SAMPLES,
        0,
    );
    let rate = rates.get("7d").copied().flatten().unwrap();
    // Weighted slope over the last 24h ≈ 0.19 %/h (≈4.6 %/d after *24).
    assert!(rate > 0.05 && rate < 1.0, "rate={rate}");
}

#[test]
fn recency_weighting_favors_recent_slope() {
    let now = crate::usage::now_ms();
    // Accelerating climb: shallow early, steep late. Strong recency weighting
    // pulls the rate well above the flat endpoint average (10→20 over 1h = 10 %/h).
    let history = vec![
        ((now - 3_600_000), make_info(Some(10.0), None)),
        ((now - 2_400_000), make_info(Some(11.0), None)),
        ((now - 1_200_000), make_info(Some(13.0), None)),
        ((now - 600_000), make_info(Some(16.0), None)),
    ];
    let five_h = make_win(20.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("5h", &five_h)],
        FIVE_H_LOOKBACK,
        MIN_SAMPLES,
        GAP_CUT,
    );
    let rate = rates.get("5h").copied().flatten().unwrap();
    // Weighted ≈ 13.2 %/h, clearly above the 10 %/h flat average.
    assert!(
        rate > 11.0,
        "rate={rate} should exceed the flat-average 10 %/h"
    );
}

// --- project_utilization (issue #8 follow-up b: burn-aware auto-switch) ---

#[test]
fn project_utilization_zero_burn_runs_flat() {
    // Idle account: burn floored at 0, so the projection is just the current
    // value regardless of the interval — "run to ~100" only via real
    // accumulation, never a phantom drop or climb.
    assert_eq!(project_utilization(42.0, 0.0, 90_000), 42.0);
}

#[test]
fn project_utilization_negative_burn_cannot_drop_projection() {
    // A negative slope (noisy fit artifact) can't project a *drop* mid-window
    // — floored at 0, same as idle.
    assert_eq!(project_utilization(50.0, -30.0, 90_000), 50.0);
}

#[test]
fn project_utilization_nan_burn_treated_as_idle() {
    // f64::max returns the non-NaN operand, so a NaN rate floors to 0 same as
    // idle rather than poisoning the projection.
    assert_eq!(project_utilization(42.0, f64::NAN, 90_000), 42.0);
}

#[test]
fn project_utilization_heavy_burn_crosses_cap_within_one_poll() {
    // 90% now, burning 1200 %/h, a 90s (0.025h) poll: 90 + 1200*0.025 = 120.
    let projected = project_utilization(90.0, 1200.0, 90_000);
    assert!((projected - 120.0).abs() < 0.01, "projected={projected}");
    assert!(
        projected >= 100.0,
        "heavy burn must cross the cap before the next poll"
    );
}

#[test]
fn project_utilization_light_burn_stays_under_cap() {
    // 90% now, a light 4 %/h burn over a 90s poll barely moves — nowhere near
    // the cap, unlike the heavy-burn case above.
    let projected = project_utilization(90.0, 4.0, 90_000);
    assert!(projected < 91.0, "projected={projected}");
    assert!(projected < 100.0);
}

#[test]
fn project_utilization_already_at_cap_stays_at_cap() {
    assert!(project_utilization(100.0, 0.0, 90_000) >= 100.0);
    assert!(project_utilization(105.0, 0.0, 90_000) >= 100.0);
}

#[test]
fn project_utilization_absurd_burn_clamps_to_finite_max() {
    // burn * hours overflows to +inf (f64::MAX * 2.0 for a 2h poll); the clamp
    // catches it instead of leaking NaN/inf into the caller's `>= 100` check.
    let projected = project_utilization(50.0, f64::MAX, 7_200_000);
    assert_eq!(projected, f64::MAX);
    assert!(projected.is_finite());
}
