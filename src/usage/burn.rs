use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::usage::{UsageInfo, UsageWindow};

/// One wallet reading from the per-profile balance series
/// (`wallet_history.jsonl`): the third-party fetch leg appends one line per
/// landing fetch for every wallet whose reading changed. A wallet's identity
/// is `(label, currency)` — the same provider can list two wallets under one
/// row label (DeepSeek's funded CNY beside an unfunded USD one), so the label
/// alone does not name a wallet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WalletSample {
    pub(crate) ts: u64,
    pub(crate) label: String,
    pub(crate) amount: f64,
    pub(crate) currency: String,
}

/// Lookback window shared by the burn-aware switch projection (`fallback.rs`)
/// and the Overview ETA line (`burn_rate_eta`) — both trust only the last hour
/// of 5h-window samples, matching the `%/h` rate they display.
pub(crate) const BURN_LOOKBACK_MS: u64 = 60 * 60 * 1000;
/// Minimum distinct samples before either consumer trusts a computed rate.
pub(crate) const BURN_MIN_SAMPLES: usize = 3;
/// Idle-gap cutoff shared by the same two consumers — an idle stretch longer
/// than this slices the history down to the active period after it.
pub(crate) const BURN_GAP_CUT_MS: u64 = 10 * 60 * 1000;

/// Lookback for the wallet-burn arm: one day, matching the currency-per-day
/// rate it displays. Retention (`profile::HISTORY_RETENTION_MS`) holds two.
pub(crate) const WALLET_BURN_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1000;
/// Minimum distinct wallet readings before the wallet rate is trusted — with
/// the bridge lines the series writer emits, that is two real balance changes.
pub(crate) const WALLET_BURN_MIN_SAMPLES: usize = 3;
/// Idle-gap cutoff for the wallet arm: a balance unchanged for longer than
/// this retires the rate (the series slices to the current reading, below the
/// floor). Equals the lookback's recency half-life, so the rate retires
/// exactly when its newest evidence has decayed past half weight.
pub(crate) const WALLET_BURN_GAP_CUT_MS: u64 = 6 * 60 * 60 * 1000;

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
/// the most recent boundary step: a utilization DROP (a window reset — the
/// pre-reset samples belong to a spent window) when `drop`, an amount RISE
/// (a top-up — a slope computed across the jump blends spend with refill and
/// reads as negative burn) when not. Returns 0 when no such step is detected.
fn step_boundary(entries: &[(u64, f64)], drop: bool) -> usize {
    for i in (1..entries.len()).rev() {
        let (prev, cur) = (entries[i - 1].1, entries[i].1);
        let crossed = if drop { cur < prev } else { cur > prev };
        if crossed {
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
        let start = step_boundary(&entries, true).max(cap);
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

/// The funded wallet's burn figure — the one derivation every surface renders
/// (the Usage tab's balance row, the overview drains line, the MCP roster row
/// and the delegate reply's headroom clause), so no two of them can disagree
/// about the same account.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WalletRate {
    /// The wallet's row label, so a surface can match the rate to the row it
    /// renders beside.
    pub(crate) label: String,
    pub(crate) currency: String,
    /// Current balance — the figure the rate qualifies.
    pub(crate) amount: f64,
    /// Recency-weighted spend, wallet currency per day; always > 0.
    pub(crate) per_day: f64,
}

/// The funded wallet's rate off its balance series: the same first-funded
/// selection the roster's rank and the headline's wallet arm make (row order,
/// never amount), so the rate always describes the balance a reader already
/// sees. `None` when no wallet is funded, the series is too thin, or the
/// balance has not moved recently enough to trust a slope.
pub(crate) fn funded_wallet_rate(
    history: &[WalletSample],
    rows: &[crate::providers::StatRow],
) -> Option<WalletRate> {
    let wallet = crate::providers::funded_wallets(rows).into_iter().next()?;
    let per_day = compute_wallet_rate_from_history(
        history,
        &wallet,
        WALLET_BURN_LOOKBACK_MS,
        WALLET_BURN_MIN_SAMPLES,
        WALLET_BURN_GAP_CUT_MS,
    )?;
    Some(WalletRate {
        label: wallet.label,
        currency: wallet.currency,
        amount: wallet.amount,
        per_day,
    })
}

/// Compute the recency-weighted wallet-burn rate (wallet currency per day)
/// for one wallet off its balance series plus the freshest reading.
///
/// The wallet arm of [`compute_burn_rates_from_history`], sharing its
/// structure: samples are filtered to this wallet's `(label, currency)`
/// identity, the current amount is appended, idle plateaus are gap-cut, flat
/// runs deduplicate, and the rate is the negated slope of a recency-weighted
/// least-squares fit (half-life `lookback_ms / 4`) over the samples inside
/// the lookback. Where the window arm cuts at the most recent utilization
/// DROP (a window reset), this arm cuts at the most recent amount RISE (a
/// top-up). `None` until `min_samples` distinct samples sit inside the
/// window, and `None` again for a non-positive rate — an idle or refilling
/// wallet shows no burn, never a negative one.
///
/// The fit runs over actual sample timestamps, never an assumed cadence: the
/// refresh interval is user-settable, so the derivation reads the spacing the
/// series itself records.
fn compute_wallet_rate_from_history(
    history: &[WalletSample],
    wallet: &crate::providers::Wallet,
    lookback_ms: u64,
    min_samples: usize,
    gap_cut_ms: u64,
) -> Option<f64> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let half_life_ms = (lookback_ms as f64 / 4.0).max(1.0);

    let mut entries: Vec<(u64, f64)> = history
        .iter()
        .filter(|s| s.label == wallet.label && s.currency == wallet.currency)
        .map(|s| (s.ts, s.amount))
        .collect();
    entries.push((now_ms, wallet.amount));

    // Must run before dedup — the equal-amount pair marks the gap.
    if gap_cut_ms > 0 && entries.len() >= 2 {
        let gap = gap_boundary(&entries, gap_cut_ms);
        if gap > 0 {
            entries = entries[gap..].to_vec();
        }
    }

    // Collapse flat runs to distinct amounts, keeping the newest timestamp of
    // each run.
    if entries.len() >= 2 {
        entries.reverse();
        entries.dedup_by(|a, b| a.1 == b.1);
        entries.reverse();
    }

    // Start no earlier than the most recent top-up, and no earlier than
    // `lookback_ms` before now (the hard sample-window cap).
    let cutoff = now_ms.saturating_sub(lookback_ms);
    let cap = entries
        .iter()
        .position(|&(ts, _)| ts >= cutoff)
        .unwrap_or(0);
    let start = step_boundary(&entries, false).max(cap);
    let recent = &entries[start..];

    if recent.len() < min_samples {
        return None;
    }
    // A wallet BURNS DOWN: the amount's slope is negative while spending, so
    // the burn rate is its negation. A positive slope survives only inside a
    // post-top-up run and reads as no burn, never negative burn.
    let per_day = -weighted_slope_per_ms(recent, half_life_ms)? * 86_400_000.0;
    (per_day > 0.0).then_some(per_day)
}

/// Slope of a recency-weighted least-squares fit of `y` over time, per ms.
/// Weights decay exponentially with age relative to the newest sample:
/// `w = 0.5^(age / half_life_ms)`, so recent samples drive the rate. Returns
/// `None` when the weighted time variance is zero (samples all simultaneous).
fn weighted_slope_per_ms(entries: &[(u64, f64)], half_life_ms: f64) -> Option<f64> {
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
    Some((sw * swxy - swx * swy) / denom)
}

/// The window arm's %/h rate: the weighted slope scaled to an hour.
fn weighted_rate_per_hour(entries: &[(u64, f64)], half_life_ms: f64) -> Option<f64> {
    weighted_slope_per_ms(entries, half_life_ms).map(|slope| slope * 3_600_000.0)
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
