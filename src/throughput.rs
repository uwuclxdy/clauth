//! Observed delegate throughput per (profile, model).
//!
//! clauth never sits in the inference request path, so the only throughput it
//! can observe is from `delegate` calls it launches itself. Each successful delegate
//! records output tokens / API duration; a short rolling window per model yields
//! a recent tokens/sec and a `degraded` flag (recent pace well below that model's
//! observed best on the same account). Subscription throttle is per-model, so a
//! profile's 5h/7d utilization gives no signal here — this fills that gap for the
//! models clauth has actually exercised. Rate-limit hits (429 / "rate limit"
//! surfaced by a failed delegate) are recorded alongside.
//!
//! Best-effort, swallow-on-error: a missing or corrupt cache reads as "no data".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::profile_cache::{load_profile_cache, write_profile_cache};

/// Per-profile throughput cache filename, relative to the per-profile dir.
pub(crate) const THROUGHPUT_CACHE_FILE: &str = "throughput_cache.json";

/// Keep at most this many most-recent samples per model.
const MAX_SAMPLES: usize = 24;
/// A model needs at least this many samples before a degraded verdict is fair.
const MIN_SAMPLES_FOR_DEGRADED: usize = 3;
/// Recent pace below this fraction of the model's observed best is "degraded".
const DEGRADED_FRACTION: f64 = 0.5;
/// A recorded rate-limit is surfaced as "recent" for this long.
const RATE_LIMIT_RECENT_SECS: i64 = 15 * 60;
/// Window size for the recency-weighted recent-pace average.
const RECENT_WINDOW: usize = 5;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ThroughputStore {
    #[serde(default)]
    models: BTreeMap<String, ModelThroughput>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ModelThroughput {
    #[serde(default)]
    samples: Vec<Sample>,
    #[serde(default)]
    best_tok_s: f64,
    #[serde(default)]
    last_rate_limited_at: Option<i64>,
    #[serde(default)]
    last_retry_after_s: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Sample {
    /// Epoch seconds the sample was recorded.
    at: i64,
    tok_s: f64,
}

/// Per-model throughput readout for display in `profiles` (roster rows and
/// the `scope: "session"` row).
#[derive(Debug, Clone)]
pub(crate) struct ModelSummary {
    pub(crate) model: String,
    pub(crate) tok_s: f64,
    pub(crate) samples: usize,
    pub(crate) degraded: bool,
    pub(crate) rate_limited_recent: bool,
    pub(crate) retry_after_s: Option<u64>,
}

/// Cache key for an unspecified model (the profile's configured default).
fn model_key(model: Option<&str>) -> String {
    model.unwrap_or("default").to_string()
}

/// Record one successful delegate run's observed pace. No-op on a zero/missing
/// duration or token count — nothing meaningful to measure.
pub(crate) fn record_success(
    profile: &crate::profile::ProfileName,
    model: Option<&str>,
    output_tokens: u64,
    duration_ms: u64,
    now: i64,
) {
    if output_tokens == 0 || duration_ms == 0 {
        return;
    }
    let tok_s = output_tokens as f64 / (duration_ms as f64 / 1000.0);
    let mut store: ThroughputStore =
        load_profile_cache(profile, THROUGHPUT_CACHE_FILE).unwrap_or_default();
    let entry = store.models.entry(model_key(model)).or_default();
    entry.samples.push(Sample { at: now, tok_s });
    let overflow = entry.samples.len().saturating_sub(MAX_SAMPLES);
    if overflow > 0 {
        entry.samples.drain(0..overflow);
    }
    if tok_s > entry.best_tok_s {
        entry.best_tok_s = tok_s;
    }
    write_profile_cache(profile, THROUGHPUT_CACHE_FILE, &store);
}

/// Record a rate-limit / 429 hit for a model, with an optional Retry-After hint.
pub(crate) fn record_rate_limit(
    profile: &crate::profile::ProfileName,
    model: Option<&str>,
    retry_after_s: Option<u64>,
    now: i64,
) {
    let mut store: ThroughputStore =
        load_profile_cache(profile, THROUGHPUT_CACHE_FILE).unwrap_or_default();
    let entry = store.models.entry(model_key(model)).or_default();
    entry.last_rate_limited_at = Some(now);
    entry.last_retry_after_s = retry_after_s;
    write_profile_cache(profile, THROUGHPUT_CACHE_FILE, &store);
}

/// Display summary for a profile's models, most-recently-sampled first. Empty
/// when the profile has no recorded runs.
pub(crate) fn summary(profile: &crate::profile::ProfileName, now: i64) -> Vec<ModelSummary> {
    let Some(store) = load_profile_cache::<ThroughputStore>(profile, THROUGHPUT_CACHE_FILE) else {
        return Vec::new();
    };
    let mut rows: Vec<(i64, ModelSummary)> = store
        .models
        .into_iter()
        .map(|(model, m)| {
            let recent = recent_avg(&m.samples);
            let degraded = m.samples.len() >= MIN_SAMPLES_FOR_DEGRADED
                && m.best_tok_s > 0.0
                && recent < m.best_tok_s * DEGRADED_FRACTION;
            let rate_limited_recent = m
                .last_rate_limited_at
                .is_some_and(|t| now - t <= RATE_LIMIT_RECENT_SECS);
            let last_at = m
                .samples
                .last()
                .map(|s| s.at)
                .or(m.last_rate_limited_at)
                .unwrap_or(0);
            (
                last_at,
                ModelSummary {
                    model,
                    tok_s: recent,
                    samples: m.samples.len(),
                    degraded,
                    rate_limited_recent,
                    retry_after_s: rate_limited_recent
                        .then_some(m.last_retry_after_s)
                        .flatten(),
                },
            )
        })
        .collect();
    rows.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    rows.into_iter().map(|(_, s)| s).collect()
}

/// Recency-weighted average of the last [`RECENT_WINDOW`] samples (linear
/// weights, newest heaviest). Empty slice → 0.
fn recent_avg(samples: &[Sample]) -> f64 {
    let take = samples.len().min(RECENT_WINDOW);
    if take == 0 {
        return 0.0;
    }
    let slice = &samples[samples.len() - take..];
    let mut weighted = 0.0;
    let mut weight = 0.0;
    for (i, s) in slice.iter().enumerate() {
        let w = (i + 1) as f64; // oldest in window = 1 … newest = take
        weighted += s.tok_s * w;
        weight += w;
    }
    weighted / weight
}

#[cfg(test)]
#[path = "../tests/inline/throughput.rs"]
mod tests;
