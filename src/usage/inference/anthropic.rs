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
//! Not read: `-representative-claim`, `-upgrade-paths`,
//! `-fallback-percentage` and the response-wide `-fallback` Claude Code
//! knows beside it, `-<name>-surpassed-threshold`, and every field of a
//! name that carries no `-utilization` (the recorded 429's `overage-status`,
//! say, which came without one). Of a name that does carry one, only its
//! `-reset` and `-status` join it: Claude Code knows an
//! `-overage-utilization`, so where that arrives `overage` is a window, and
//! the `-overage-in-use`, `-overage-scope`, `-overage-disabled-reason` and
//! `-overage-period` fields stay out all the same. Out too is the
//! low-priority lane's own state (`-slow-status`, `-slow-offer`,
//! `-slow-max-wait`, `-slow-retry-after`), which is no window and waits for
//! the probe that asks for that lane. The `-overage-period-*` sub-pools are
//! another matter: they carry a `-utilization` each, so they arrive as windows
//! of their own, and keeping them out of the store's limits is the mapping's
//! job. A status without any window is no reading either: for the kick its
//! 429 handling (`kick_rate_limit_at`) owns that verdict, together with
//! `retry-after`, and the probe reads such a 429 as a throttle.
//!
//! The probe asks for the reading: the kick's 1-token message on [`MODEL`],
//! sent with the kick's header set and read for its headers, never its body.

use std::time::Duration;

use super::{InferenceReading, InferenceWindow, LimitStatus};
use crate::usage::{
    ANTHROPIC_ORIGIN, UsageInfo, UsageWindow, await_request_slot, epoch_secs_to_iso,
    now_epoch_secs, parse_retry_after_at,
};

/// The model of every messages request clauth sends on its own behalf, the
/// probe and the auto-start kick alike, so the two cannot drift apart. The
/// cheapest one: a probe only wants the response's headers.
pub(crate) const MODEL: &str = "claude-haiku-5-5";

const PREFIX: &str = "anthropic-ratelimit-unified-";
const UTILIZATION: &str = "-utilization";

/// The reading in `headers`, the response's whole header set as name/value
/// pairs. `None` when it reports no window, so a response without the headers
/// never stands in for a reading. A value that does not parse drops that one
/// field (a window without a usable utilization drops the window), never the
/// reading. Pure: the caller owns the response.
pub(crate) fn reading_from_headers<'h>(
    headers: impl IntoIterator<Item = (&'h str, &'h str)>,
) -> Option<InferenceReading> {
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
    let windows: Vec<InferenceWindow> = unified
        .iter()
        .filter_map(|(name, value)| {
            let window = name.strip_suffix(UTILIZATION).filter(|w| !w.is_empty())?;
            Some(InferenceWindow {
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
    Some(InferenceReading {
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
fn status(value: &str) -> Option<LimitStatus> {
    [
        ("allowed", LimitStatus::Allowed),
        ("allowed_warning", LimitStatus::Warning),
        ("rejected", LimitStatus::Rejected),
    ]
    .into_iter()
    .find(|(word, _)| value.eq_ignore_ascii_case(word))
    .map(|(_, status)| status)
}

/// What one probe came back with.
#[derive(Debug, Clone)]
pub(crate) enum ProbeAnswer {
    /// The response carried a reading. A 429 can: the limiter rejects a spent
    /// window with the same header set a 2xx carries. Boxed: `UsageInfo`
    /// dwarfs every other variant here (clippy::large_enum_variant).
    Reading(Box<UsageInfo>),
    /// A 429 without a reading: the endpoint throttled the request and said
    /// nothing about the account. `retry_after` as the server sent it.
    RateLimited { retry_after: Option<Duration> },
    /// 401 or 403: the server refused the token, which only a new token
    /// changes.
    Rejected,
    /// Anything else: another status, a 2xx without the headers, or no
    /// response at all.
    Failed,
}

/// Probe the account `access_token` belongs to. Takes its turn on the
/// `api.anthropic.com` request-spacing slot `/usage` and the kick share.
pub(crate) fn probe(access_token: &str) -> ProbeAnswer {
    probe_to(crate::oauth::messages_endpoint().as_ref(), access_token)
}

/// [`probe`] against `url`, so a loopback listener can pin what it sends.
pub(crate) fn probe_to(url: &str, access_token: &str) -> ProbeAnswer {
    await_request_slot(ANTHROPIC_ORIGIN);
    let Ok(body) = serde_json::to_string(&serde_json::json!({
        "model": MODEL,
        "max_tokens": 1,
        "system": [{ "type": "text", "text": crate::oauth::KICK_SYSTEM_PROMPT }],
        "messages": [{ "role": "user", "content": "x" }],
    })) else {
        return ProbeAnswer::Failed;
    };
    let sent = crate::oauth::http_agent()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", crate::oauth::KICK_ANTHROPIC_BETA)
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("User-Agent", crate::usage::cli_user_agent())
        .header("x-app", "cli")
        .header("x-stainless-lang", "js")
        .header("x-stainless-runtime", "node")
        .header(
            "x-stainless-package-version",
            crate::oauth::KICK_STAINLESS_PACKAGE_VERSION,
        )
        .send(&body);
    let Ok(response) = sent else {
        return ProbeAnswer::Failed;
    };
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
    answer_from(response.status().as_u16(), headers, now_epoch_secs())
}

/// The answer one response gives, off its status and whole header set. Pure,
/// so every arm is testable without HTTP; `now_secs` anchors a date-form
/// `retry-after`.
fn answer_from<'h>(
    status: u16,
    headers: impl IntoIterator<Item = (&'h str, &'h str)>,
    now_secs: i64,
) -> ProbeAnswer {
    match status {
        401 | 403 => return ProbeAnswer::Rejected,
        200..=299 | 429 => {}
        _ => return ProbeAnswer::Failed,
    }
    let headers: Vec<(&str, &str)> = headers.into_iter().collect();
    // Only `5h` and `7d` map into the store, so windows without either would
    // land as an empty reading over the last good one.
    if let Some(info) = reading_from_headers(headers.iter().copied())
        .map(|reading| usage_from_reading(&reading))
        .filter(|info| info.five_hour.is_some() || info.seven_day.is_some())
    {
        return ProbeAnswer::Reading(Box::new(info));
    }
    if status != 429 {
        return ProbeAnswer::Failed;
    }
    let retry_after = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| parse_retry_after_at(value, now_secs));
    ProbeAnswer::RateLimited { retry_after }
}

/// The store's reading: `5h` and `7d` become the two windows every surface and
/// the walk read, each reset in the ISO form `/usage` sends. The other windows
/// stay out until a probe has shown which store window each one is, so the
/// walk never judges on a pool it cannot name.
fn usage_from_reading(reading: &InferenceReading) -> UsageInfo {
    let window = |name: &str| {
        reading
            .windows
            .iter()
            .find(|w| w.name == name)
            .map(|w| UsageWindow {
                utilization: w.utilization,
                resets_at: w.resets_at.map(epoch_secs_to_iso),
            })
    };
    UsageInfo {
        five_hour: window("5h"),
        seven_day: window("7d"),
        ..UsageInfo::default()
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_inference_anthropic.rs"]
mod tests;
