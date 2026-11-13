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
//! [`send`] is the one messages request clauth sends on its own behalf, the
//! kick's 1-token message on [`MODEL`]. The probe reads its answer off the
//! headers, and off the body only what a 401 or a 403 refuses: the
//! organization, the token, or neither.

use std::time::Duration;

use super::{InferenceReading, InferenceWindow, LimitStatus};
use crate::usage::{
    ANTHROPIC_ORIGIN, UsageInfo, UsageWindow, await_request_slot, epoch_secs_to_iso,
    now_epoch_secs, parse_retry_after_at,
};

/// The model of every messages request clauth sends on its own behalf, the
/// probe and the auto-start kick alike: the cheapest one, and the one Claude
/// Code's own 1-token quota probe asks on.
pub(crate) const MODEL: &str = "claude-haiku-4-5-20251001";

/// How long a refusal's body may take and how much of it is read: the code
/// sits in a few hundred bytes, and a body that stalls must not hold the
/// probe leg's one thread. The agent's whole-call bound
/// ([`crate::oauth::TOKEN_HTTP_DEADLINES`], 19 s) ends such a call anyway;
/// this tighter one frees the thread sooner.
const REFUSAL_BODY_TIMEOUT: Duration = Duration::from_secs(5);
const REFUSAL_BODY_LIMIT: u64 = 16 * 1024;

/// The error codes Claude Code reads as the organization's refusal rather than
/// the token's: an organization that may not use OAuth (a canceled
/// subscription among them), a key whose creator left the organization, an
/// endpoint closed to its region, an organization on hold or disabled, and an
/// address outside the organization's allowed range. A new token fixes none
/// of them.
const ORG_REFUSED_CODES: [&str; 6] = [
    "oauth_not_allowed_for_organization",
    "claude_code_key_creator_not_member",
    "restricted_regime_endpoint",
    "organization_on_hold",
    "organization_disabled",
    "ip_not_in_allowed_range",
];

/// The one organization code a canceled subscription answers OAuth
/// `/v1/messages` with, seen on pair-token accounts: it lands as a canceled
/// plan, every other organization code as the organization's refusal.
const CANCELED_CODE: &str = "oauth_not_allowed_for_organization";

/// The error codes Claude Code reads as the credential's own refusal. A
/// `permission_error` refuses the token only with one of them or with
/// [`REVOKED_MESSAGE`]: without either it may refuse anything else about the
/// request.
const CREDENTIAL_REFUSED_CODES: [&str; 4] = [
    "token_expired",
    "token_revoked",
    "token_invalid",
    "credential_invalid",
];

/// The message Claude Code reads as a revoked OAuth token on its own, with no
/// credential code beside it.
const REVOKED_MESSAGE: &str = "OAuth token has been revoked";

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

/// What one messages request came back with: its status, its whole header set
/// in the order it arrived, and on a 401 or a 403 what its body refuses.
#[derive(Debug, Clone)]
pub(crate) struct MessagesResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) forbidden: Option<Forbidden>,
}

/// What a 401's or a 403's body refuses ([`forbidden_by`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forbidden {
    /// One of [`ORG_REFUSED_CODES`]: the organization, not the token.
    /// `canceled` for [`CANCELED_CODE`], the code a canceled subscription
    /// answers with.
    Organization { canceled: bool },
    /// An Anthropic error body refusing the credential.
    Token,
    /// Anything else: a WAF, region or TLS-intercepting proxy's page, an empty
    /// body, an unknown shape. It says nothing about the token.
    Unrecognized,
}

impl MessagesResponse {
    /// The first value of `name`, matched in any case, as `HeaderMap::get`
    /// would have it.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(seen, _)| seen.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Send the 1-token message on [`MODEL`] with Claude Code's `/v1/messages`
/// header set: the one request the kick and the probe both send. It waits its
/// turn on the `api.anthropic.com` spacing slot `/usage` shares and calls
/// `on_turn` once that wait is over, so a caller shows the request in flight
/// only then. `on_turn` doubles as the last ticket check: returning `false`
/// skips the POST and answers `Ok(None)` — the request was never sent, so no
/// caller records a verdict for it. The `system` prefix stays: an OAuth token
/// without it is refused as non-Claude-Code inference. The body is read only
/// on a 401 or a 403, for what it refuses ([`forbidden_by`]), and never
/// leaves this function.
pub(crate) fn send(
    url: &str,
    access_token: &str,
    on_turn: impl FnOnce() -> bool,
) -> Result<Option<MessagesResponse>, ureq::Error> {
    await_request_slot(ANTHROPIC_ORIGIN);
    if !on_turn() {
        return Ok(None);
    }
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 1,
        "system": [{ "type": "text", "text": crate::oauth::KICK_SYSTEM_PROMPT }],
        "messages": [{ "role": "user", "content": "x" }],
    })
    .to_string();
    let mut response = crate::oauth::http_agent()
        .post(url)
        .config()
        .timeout_recv_body(Some(REFUSAL_BODY_TIMEOUT))
        .build()
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
        .send(&body)?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect();
    let forbidden = matches!(status, 401 | 403).then(|| {
        response
            .body_mut()
            .with_config()
            .limit(REFUSAL_BODY_LIMIT)
            .read_to_string()
            .map_or(Forbidden::Unrecognized, |body| forbidden_by(&body))
    });
    Ok(Some(MessagesResponse {
        status,
        headers,
        forbidden,
    }))
}

/// What a 401's or a 403's body refuses. The organization when it names one of
/// [`ORG_REFUSED_CODES`], read where Claude Code reads an error code:
/// `error.details.error_code`, else `error.error_code`, else `error.code`. The
/// token when it is an Anthropic error body (`"type": "error"`) whose
/// `error.type` is `authentication_error`, or `permission_error` with one of
/// [`CREDENTIAL_REFUSED_CODES`] or an `error.message` saying
/// [`REVOKED_MESSAGE`]. Anything else is unrecognized: a 401 or a 403 from a
/// proxy, a WAF or a region block in front of the API carries no such body,
/// and a token it calls dead would stay dead across every restart.
fn forbidden_by(body: &str) -> Forbidden {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Forbidden::Unrecognized;
    };
    let error = &value["error"];
    let code = [
        &error["details"]["error_code"],
        &error["error_code"],
        &error["code"],
    ]
    .into_iter()
    .find_map(serde_json::Value::as_str);
    if let Some(code) = code.filter(|code| ORG_REFUSED_CODES.contains(code)) {
        return Forbidden::Organization {
            canceled: code == CANCELED_CODE,
        };
    }
    let refuses_token = value["type"].as_str() == Some("error")
        && match error["type"].as_str() {
            Some("authentication_error") => true,
            Some("permission_error") => {
                code.is_some_and(|code| CREDENTIAL_REFUSED_CODES.contains(&code))
                    || error["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(REVOKED_MESSAGE))
            }
            _ => false,
        };
    if refuses_token {
        Forbidden::Token
    } else {
        Forbidden::Unrecognized
    }
}

/// What one probe came back with.
#[derive(Debug, Clone)]
pub(crate) enum ProbeAnswer {
    /// The response carried a reading. A 429 can: the limiter rejects a spent
    /// window with the same header set a 2xx carries.
    Reading {
        /// Boxed: `UsageInfo` dwarfs every other variant here
        /// (clippy::large_enum_variant).
        info: Box<UsageInfo>,
        /// The server's `retry-after`, which holds for a reading as for a
        /// throttle.
        retry_after: Option<Duration>,
        /// On a 429 whose windows all read below the cap, the limiter's verdict
        /// as the kick reads its own: such windows do not show an account the
        /// limiter refuses. A spent window shows it, so it carries none.
        limited: Option<crate::oauth::KickRateLimit>,
    },
    /// A 429 without a reading: the endpoint throttled the request and said
    /// nothing about the account. `retry_after` as the server sent it.
    RateLimited { retry_after: Option<Duration> },
    /// A 401 or a 403 whose Anthropic error body refuses the credential, or a
    /// 401 whose body names an organization code: the server refused the
    /// token, which only a new token changes.
    Rejected,
    /// A 403 naming the organization, not the token, which a new token cannot
    /// fix: `canceled` for a canceled subscription ([`CANCELED_CODE`]), else
    /// an organization on hold, disabled or out of reach.
    OrgRefused { canceled: bool },
    /// Anything else: another status, a 401 or a 403 with a body that is no
    /// Anthropic refusal, a 2xx without the headers, or no response at all.
    Failed,
}

/// Probe the account `access_token` belongs to, calling `on_turn` once the
/// request leaves the spacing slot. `None` when `on_turn` refused: the
/// request was never sent, which reads as no answer, never as a verdict a
/// caller records.
pub(crate) fn probe(access_token: &str, on_turn: impl FnOnce() -> bool) -> Option<ProbeAnswer> {
    match send(
        crate::oauth::messages_endpoint().as_ref(),
        access_token,
        on_turn,
    ) {
        Ok(Some(response)) => Some(answer_from(
            response.status,
            response.forbidden,
            response
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            now_epoch_secs(),
        )),
        Ok(None) => None,
        Err(_) => Some(ProbeAnswer::Failed),
    }
}

/// The answer one response gives, off its status, what a 401's or a 403's body
/// refuses and its whole header set. Pure, so every arm is testable without HTTP;
/// `now_secs` anchors a date-form `retry-after`.
fn answer_from<'h>(
    status: u16,
    forbidden: Option<Forbidden>,
    headers: impl IntoIterator<Item = (&'h str, &'h str)>,
    now_secs: i64,
) -> ProbeAnswer {
    match (status, forbidden) {
        // A 401 is the token's own refusal whatever code its body names, once
        // the body is the server's at all.
        (401, Some(Forbidden::Token | Forbidden::Organization { .. }))
        | (403, Some(Forbidden::Token)) => {
            return ProbeAnswer::Rejected;
        }
        (403, Some(Forbidden::Organization { canceled })) => {
            return ProbeAnswer::OrgRefused { canceled };
        }
        (200..=299 | 429, _) => {}
        _ => return ProbeAnswer::Failed,
    }
    let headers: Vec<(&str, &str)> = headers.into_iter().collect();
    let header = |want: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(want))
            .map(|(_, value)| *value)
    };
    let retry_after = header("retry-after").and_then(|value| parse_retry_after_at(value, now_secs));
    // Only `5h` and `7d` map into the store, so windows without either would
    // land as an empty reading over the last good one.
    if let Some(info) = reading_from_headers(headers.iter().copied())
        .map(|reading| usage_from_reading(&reading))
        .filter(|info| info.five_hour.is_some() || info.seven_day.is_some())
    {
        // A window the reading shows spent says itself why the limiter refuses,
        // and the walk reads it so; the kick-block axis is for a refusal the
        // windows below the cap do not show.
        let below_cap = [&info.five_hour, &info.seven_day]
            .into_iter()
            .flatten()
            .all(|window| window.utilization < 100.0);
        let limited = (status == 429 && below_cap).then(|| {
            crate::oauth::kick_rate_limit_at(
                header("anthropic-ratelimit-unified-status"),
                header("anthropic-ratelimit-unified-reset"),
                header("retry-after"),
                now_secs,
            )
        });
        return ProbeAnswer::Reading {
            info: Box::new(info),
            retry_after,
            limited,
        };
    }
    if status != 429 {
        return ProbeAnswer::Failed;
    }
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
