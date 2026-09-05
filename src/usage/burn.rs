use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::usage::{UsageInfo, UsageWindow};

/// Lookback window shared by the burn-aware switch projection (`fallback.rs`)
/// and the Overview ETA line (`burn_rate_eta`) — both trust only the last hour
/// of 5h-window samples, matching the `%/h` rate they display.
pub(crate) const BURN_LOOKBACK_MS: u64 = 60 * 60 * 1000;
/// Minimum distinct samples before either consumer trusts a computed rate.
pub(crate) const BURN_MIN_SAMPLES: usize = 3;
/// Idle-gap cutoff shared by the same two consumers — an idle stretch longer
/// than this slices the history down to the active period after it.
pub(crate) const BURN_GAP_CUT_MS: u64 = 10 * 60 * 1000;

/// Utilization of the window named `label` in this snapshot, or `None` if the
/// snapshot has no such window. Resolves dynamically against [`UsageInfo::windows`]
/// so per-model labels (`"7d fable"`, …) work without a hardcoded arm.
fn window_util(usage: &UsageInfo, label: &str) -> Option<f64> {
    usage
        .windows()
        .into_iter()
        .find(|(l, _)| *l == label)
        .map(|(_, w)| w.utilization)
}

/// Walk `entries` (chronological) to find the index of the first entry after
/// the most recent utilization drop (window reset). Returns 0 when no reset is
/// detected.
fn reset_boundary(entries: &[(u64, f64)]) -> usize {
    for i in (1..entries.len()).rev() {
        if entries[i].1 < entries[i - 1].1 {
            return i;
        }
    }
    0
}

/// Walk `entries` (chronological) to find the most recent pair of consecutive
/// entries with identical utilization whose time gap exceeds `max_gap_ms`.
/// Returns the index of the later entry (start of the active period after an
/// idle stretch), or 0 when no idle gap is detected.
fn gap_boundary(entries: &[(u64, f64)], max_gap_ms: u64) -> usize {
    for i in (1..entries.len()).rev() {
        if entries[i].1 == entries[i - 1].1
            && entries[i].0.saturating_sub(entries[i - 1].0) > max_gap_ms
        {
            return i;
        }
    }
    0
}

/// Compute recency-weighted burn rates (%/h) per usage window from cached
/// history plus the latest current usage.
///
/// `windows` is a slice of `(label, &UsageWindow)` pairs — typically a subset
/// of [`UsageInfo::windows`].
///
/// For each window the samples are filtered to that window's utilization, the
/// current value is appended, idle plateaus are gap-cut, and flat runs are
/// deduplicated to distinct utilization points. The rate is the slope of a
/// recency-weighted least-squares fit over the samples falling within the last
/// `lookback_ms` (and after the most recent window reset). Sample weights decay
/// exponentially with age — half-life `lookback_ms / 4` — so the newest samples
/// dominate. `None` is returned until at least `min_samples` distinct samples
/// sit inside that window, so a rate is never shown from too little data.
///
/// `lookback_ms` is a hard cap: samples older than `now - lookback_ms` are
/// dropped (1 h for the 5-hour window → `%/h`).
///
/// `gap_cut_ms` controls idle-gap detection: when two consecutive entries share
/// the same utilization and their timestamps differ by more than `gap_cut_ms`,
/// the history is sliced from the later entry onward. Pass 0 to disable gap-cut
/// entirely (for windows where idle stretches should count toward the rate).
///
/// Returns rates in %/h. Drives the 5-hour window's `%/h` rate and the overview
/// burn-ETA. The 7-day windows show a window-anchored average pace instead
/// (`window_avg_pace_per_day`): utilization is whole percents, so a week's series
/// yields distinct samples far too slowly to slope, and the log holds only 2 days
/// either way. Not for the reason this comment used to give — the log lives under
/// `profiles/<name>/` and is read by name, so it never carries another account's
/// numbers.
pub(crate) fn compute_burn_rates_from_history(
    history: &[(u64, UsageInfo)],
    windows: &[(&str, &UsageWindow)],
    lookback_ms: u64,
    min_samples: usize,
    gap_cut_ms: u64,
) -> HashMap<String, Option<f64>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let half_life_ms = (lookback_ms as f64 / 4.0).max(1.0);

    let mut rates = HashMap::new();
    for (label, window) in windows {
        let mut entries: Vec<(u64, f64)> = history
            .iter()
            .filter_map(|(ts, u)| window_util(u, label).map(|p| (*ts, p)))
            .collect();

        entries.push((now_ms, window.utilization));

        // Must run before dedup — the equal-util pair marks the gap.
        if gap_cut_ms > 0 && entries.len() >= 2 {
            let gap = gap_boundary(&entries, gap_cut_ms);
            if gap > 0 {
                entries = entries[gap..].to_vec();
            }
        }

        // Collapse flat runs to distinct utilization points, keeping the newest
        // timestamp of each run.
        if entries.len() >= 2 {
            entries.reverse();
            entries.dedup_by(|a, b| a.1 == b.1);
            entries.reverse();
        }

        // Start no earlier than the most recent window reset, and no earlier
        // than `lookback_ms` before now (the hard sample-window cap).
        let cutoff = now_ms.saturating_sub(lookback_ms);
        let cap = entries
            .iter()
            .position(|&(ts, _)| ts >= cutoff)
            .unwrap_or(0);
        let start = reset_boundary(&entries).max(cap);
        let recent = &entries[start..];

        // Require enough distinct samples in the window before trusting a rate.
        let rate = if recent.len() >= min_samples {
            weighted_rate_per_hour(recent, half_life_ms)
        } else {
            None
        };
        rates.insert(label.to_string(), rate);
    }
    rates
}

/// Slope of a recency-weighted least-squares fit of utilization over time, in
/// %/h. Weights decay exponentially with age relative to the newest sample:
/// `w = 0.5^(age / half_life_ms)`, so recent samples drive the rate. Returns
/// `None` when the weighted time variance is zero (samples all simultaneous).
fn weighted_rate_per_hour(entries: &[(u64, f64)], half_life_ms: f64) -> Option<f64> {
    let last_ts = entries[entries.len() - 1].0;
    let base = entries[0].0 as f64; // rebase time for numeric stability

    let (mut sw, mut swx, mut swy, mut swxx, mut swxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(ts, util) in entries {
        let age = last_ts.saturating_sub(ts) as f64;
        let w = 0.5_f64.powf(age / half_life_ms);
        let x = ts as f64 - base;
        sw += w;
        swx += w * x;
        swy += w * util;
        swxx += w * x * x;
        swxy += w * x * util;
    }

    let denom = sw * swxx - swx * swx;
    if denom.abs() < f64::EPSILON {
        return None;
    }
    let slope_per_ms = (sw * swxy - swx * swy) / denom;
    Some(slope_per_ms * 3_600_000.0)
}

/// Projects a window's utilization over a look-ahead: current value plus the
/// recent burn rate (%/h, from [`compute_burn_rates_from_history`]) times
/// `interval_ms`. The helper takes the interval it is given; callers cap the
/// horizon themselves where they want one — `fallback::projected_exhausted`
/// folds in `min(interval_ms, horizon_cap_ms)`, and the headroom nudge passes
/// the raw window remainder uncapped. Burn is floored at 0 — an idle or negative rate
/// can't project a utilization *drop* mid-window, so an idle account simply
/// projects flat at its current value ("run to ~100" only via real
/// accumulation). The result is clamped finite so a corrupt or extreme rate
/// can't produce NaN/overflow at the caller's `>= 100` comparison.
///
/// Drives the opt-in burn-aware auto-switch decision (issue #8 follow-up b,
/// `fallback::is_exhausted_projected`): switch the ACTIVE profile when the
/// projected value crosses the 100% cap, instead of waiting for the static
/// per-profile threshold.
pub(crate) fn project_utilization(util_pct: f64, burn_pct_per_hour: f64, interval_ms: u64) -> f64 {
    let hours = interval_ms as f64 / 3_600_000.0;
    let projected = util_pct + burn_pct_per_hour.max(0.0) * hours;
    if projected.is_finite() {
        projected
    } else {
        f64::MAX
    }
}

#[cfg(test)]
#[path = "../../tests/inline/burn.rs"]
mod tests;
