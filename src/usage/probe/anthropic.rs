//! Anthropic's usage reading: the `anthropic-ratelimit-unified-*` headers
//! every `/v1/messages` response carries, 2xx and 429 alike, whatever scopes
//! the token holds.
//!
//! Per window `<name>` the response carries `-<name>-utilization` (a
//! fraction, 0..1), mostly with `-<name>-reset` (epoch seconds) and
//! `-<name>-status` (`allowed`, `allowed_warning`, `rejected`), next to a
//! response-wide `-status` and `-reset`. Seen on the wire (2026-10-07): `5h`
//! and `7d` on every response, `7d_oi` only when the request's model counts
//! into it (Fable yes, Haiku no). Claude Code 2.1.294 reads `7d_oi` as
//! `seven_day_overage_included`, and also knows `overage` (with its
//! `overage-period-*`), `grace-5h` and `grace-7d` (no reset of their own) and
//! the `slow-budget` of a low-priority request. The parse keys on the
//! `-utilization` suffix rather than on a list of names, so all of these, and
//! whatever comes next, arrive as windows without a code change; which of them
//! count as rate windows is the store mapping's call.
//!
//! The response-wide `-reset` is the binding window's reset; on a 429 it is an
//! upper bound only (the limiter has relented early, see `KickRateLimit`).
//!
//! Not read: `-representative-claim`, `-fallback-percentage`,
//! `-upgrade-paths`, `-<name>-surpassed-threshold`, any field of a name that
//! carries no `-utilization` (a 429's `overage-status`, say), and the
//! low-priority lane's own state (`-slow-status`, `-slow-offer`,
//! `-slow-max-wait`, `-slow-retry-after`), which is no window and waits for the
//! probe that asks for that lane. A status without any window is no reading
//! either: the kick's 429 handling (`kick_rate_limit_at`) owns that verdict,
//! together with `retry-after`.

use super::{ProbeReading, ProbeStatus, ProbeWindow};

const PREFIX: &str = "anthropic-ratelimit-unified-";
const UTILIZATION: &str = "-utilization";

/// The reading in `headers`, the response's whole header set as name/value
/// pairs. `None` when it reports no window, so a response without the headers
/// never stands in for a reading. A value that does not parse drops that one
/// field (a window without a usable utilization drops the window), never the
/// reading. Pure: the caller owns the response.
pub(crate) fn reading_from_headers<'h>(
    headers: impl IntoIterator<Item = (&'h str, &'h str)>,
) -> Option<ProbeReading> {
    // Header names are case-insensitive on the wire. The first of a repeated
    // name wins, usable or not, as `HeaderMap::get` would have it.
    let mut unified: Vec<(String, &str)> = Vec::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if let Some(rest) = name.strip_prefix(PREFIX)
            && !unified.iter().any(|(seen, _)| seen == rest)
        {
            unified.push((rest.to_string(), value.trim()));
        }
    }
    let field = |key: &str| {
        unified
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| *value)
    };
    let windows: Vec<ProbeWindow> = unified
        .iter()
        .filter_map(|(name, value)| {
            let window = name.strip_suffix(UTILIZATION).filter(|w| !w.is_empty())?;
            Some(ProbeWindow {
                name: window.to_string(),
                utilization: percent(value)?,
                resets_at: field(&format!("{window}-reset")).and_then(epoch_secs),
                status: field(&format!("{window}-status")).and_then(status),
            })
        })
        .collect();
    if windows.is_empty() {
        return None;
    }
    Some(ProbeReading {
        windows,
        status: field("status").and_then(status),
        resets_at: field("reset").and_then(epoch_secs),
    })
}

/// A header's fraction as a percentage. Binary floats put `0.29 * 100` at
/// 28.999999999999996, and the walk compares `>=` against whole-number lines,
/// so the product is rounded to a millionth of a percent. `None` for anything
/// but a finite, non-negative number.
fn percent(value: &str) -> Option<f64> {
    let pct = (value.parse::<f64>().ok()? * 100.0 * 1e6).round() / 1e6;
    (pct.is_finite() && pct >= 0.0).then_some(pct)
}

fn epoch_secs(value: &str) -> Option<i64> {
    value.parse().ok()
}

/// The three statuses Claude Code 2.1.294 knows. Any other value reads as no
/// status rather than as one of them.
fn status(value: &str) -> Option<ProbeStatus> {
    [
        ("allowed", ProbeStatus::Allowed),
        ("allowed_warning", ProbeStatus::Warning),
        ("rejected", ProbeStatus::Rejected),
    ]
    .into_iter()
    .find(|(word, _)| value.eq_ignore_ascii_case(word))
    .map(|(_, status)| status)
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_probe_anthropic.rs"]
mod tests;
