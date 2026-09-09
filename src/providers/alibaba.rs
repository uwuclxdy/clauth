//! Alibaba Model Studio (Bailian) — Token Plan Solo/Personal usage.
//!
//! Unlike every other provider here, the profile's api key CANNOT read quota:
//! sending it as a bearer returns byte-identical output to sending no auth
//! header at all. The only credential the quota surface reads is a **console**
//! session ([`crate::profile::ConsoleCredential`]), captured by
//! `crate::alibaba_login` and stored in the profile's `[console]` table.
//!
//! Three APIs behind the OneConsole RPC gateway, each a form-encoded POST whose
//! `Data` is empty apart from the mandatory `cornerstoneParam`:
//! - `…/api/v2/usage` → `per1WeekPercentage` (a RATIO in 0..1) + reset stamp,
//!   and the documented-but-unobserved `per5Hour*` siblings.
//! - `…/api/v2/subscription` → tier (`specCode`), status, remaining days.
//! - `…/api/v2/quota-config` → per-tier absolute `five_hour` / `weekly`
//!   allowances, which turn the percentages into `used / total`.
//!
//! Every response is HTTP 200, success or not, so the verdict is read out of the
//! body — including the dead-session code, which is a first-class state
//! ([`ThirdPartyError::AuthExpired`]) rather than a fetch failure: the session
//! has no refresh path — it expires 48h after the operator's aliyun browser
//! sign-in, which a re-login inherits rather than restarts — so retrying it on
//! the cadence only burns requests.

use std::collections::HashMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, UsageBar};
use crate::oauth_login::percent_encode;
use crate::profile::{ConsoleCredential, ConsoleSite};
use crate::usage::epoch_secs_to_iso;

pub(super) const DISPLAY_NAME: &str = "Alibaba Model Studio";

/// Region assumed when a profile's `[console]` table carries none. Also the row
/// the gateway table falls back to for any region it doesn't know.
pub(crate) const DEFAULT_REGION: &str = "cn-beijing";

/// The international deployment's region id — the one the intl inference hosts
/// sit in, used when deriving a console site/region from a `base_url`.
const INTL_REGION: &str = "ap-southeast-1";

/// The inference hosts clauth recognises as Alibaba Model Studio, each with the
/// console site + region its plan is administered from, and the console page
/// where that plan's api key and quota live. These are the four the shipped
/// presets point at; matching is host-boundary (`url_matches_host`), so
/// `…aliyuncs.com.evil.tld` never claims one.
///
/// The page is per HOST, not per site: Token Plan and Coding Plan are separate
/// products administered from separate console routes, and each front spells the
/// route differently. Every one is the vendor's own published deep link (three
/// off Alibaba's docs, the domestic Token Plan one off the `bl` CLI's bundle),
/// never a guess extrapolated from a sibling.
const HOSTS: &[(&str, ConsoleSite, &str, &str)] = &[
    (
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com",
        ConsoleSite::International,
        INTL_REGION,
        "https://modelstudio.console.alibabacloud.com/ap-southeast-1?tab=plan#/efm/subscription/overview",
    ),
    (
        "https://token-plan.cn-beijing.maas.aliyuncs.com",
        ConsoleSite::Domestic,
        DEFAULT_REGION,
        "https://bailian.console.aliyun.com/cn-beijing?tab=plan#/efm/subscription/overview",
    ),
    (
        "https://coding-intl.dashscope.aliyuncs.com",
        ConsoleSite::International,
        INTL_REGION,
        "https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=globalset#/efm/coding_plan",
    ),
    (
        "https://coding.dashscope.aliyuncs.com",
        ConsoleSite::Domestic,
        DEFAULT_REGION,
        "https://bailian.console.aliyun.com/cn-beijing/?tab=plan#/efm/subscription/coding-plan",
    ),
];

const USAGE_API: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage";
const SUBSCRIPTION_API: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription";
const QUOTA_CONFIG_API: &str = "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config";

/// Body codes that mean "this console session is dead", not "the request was
/// wrong". Both arrive under HTTP 200. `ConsoleNeedLogin` is the sibling front's
/// spelling of the same verdict.
const LOGIN_ERROR_CODES: &[&str] = &["BailianGateway.Login.NotLogined", "ConsoleNeedLogin"];

pub(super) fn matches_base_url(url: &str) -> bool {
    HOSTS
        .iter()
        .any(|(host, _, _, _)| super::url_matches_host(url, host))
}

/// The console site + region an Alibaba `base_url` is administered from, so a
/// login can open the right console without asking. `None` for a URL this
/// module doesn't recognise.
pub(crate) fn site_and_region(base_url: &str) -> Option<(ConsoleSite, &'static str)> {
    HOSTS
        .iter()
        .find(|(host, _, _, _)| super::url_matches_host(base_url, host))
        .map(|(_, site, region, _)| (*site, *region))
}

/// The console page this endpoint's plan is administered from — where its api
/// key is minted and its quota is shown. `None` for a URL this module doesn't
/// recognise, which is the same input [`matches_base_url`] rejects, so a caller
/// that reached here through [`super::Provider::from_base_url`] always gets a
/// page rather than relying on a fallback to be right.
pub(super) fn console_url(base_url: &str) -> Option<&'static str> {
    HOSTS
        .iter()
        .find(|(host, _, _, _)| super::url_matches_host(base_url, host))
        .map(|(_, _, _, page)| *page)
}

/// One console gateway: the host that answers, and the RPC action it answers
/// under. Both are picked by (region, site) together — the international
/// deployment uses a different action name than the mainland one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gateway {
    pub(crate) origin: &'static str,
    action: &'static str,
}

/// Gateway for a (region, site) pair. An unknown region falls back to the
/// `cn-beijing` row **for that site**: the site is the one axis that can't be
/// guessed (a token minted on one front is meaningless on the other), while a
/// region clauth doesn't know is most likely a new mainland one.
pub(crate) fn gateway(region: &str, site: ConsoleSite) -> Gateway {
    match (region, site) {
        (INTL_REGION, ConsoleSite::Domestic) => Gateway {
            origin: "https://modelstudio-cs.console.aliyun.com",
            action: "IntlBroadScopeAspnGateway",
        },
        (INTL_REGION, ConsoleSite::International) => Gateway {
            origin: "https://bailian-singapore-cs.alibabacloud.com",
            action: "IntlBroadScopeAspnGateway",
        },
        (_, ConsoleSite::Domestic) => Gateway {
            origin: "https://bailian-cs.console.aliyun.com",
            action: "BroadScopeAspnGateway",
        },
        (_, ConsoleSite::International) => Gateway {
            origin: "https://bailian-cs.console.alibabacloud.com",
            action: "BroadScopeAspnGateway",
        },
    }
}

/// Per-host request-pacing key for a profile ([`super::ThirdPartyTarget::throttle_key`]).
/// With no credential no request is ever made, so the default row's host is a
/// stable placeholder rather than a claim about where this profile would go.
pub(crate) fn gateway_origin(console: Option<&ConsoleCredential>) -> &'static str {
    match console {
        Some(c) => gateway(&c.region, c.site).origin,
        None => gateway(DEFAULT_REGION, ConsoleSite::default()).origin,
    }
}

pub(super) fn fetch(
    console: Option<&ConsoleCredential>,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    // No session is the same state to every consumer as a dead one: both need a
    // console login and neither is worth a request.
    let console = console.ok_or(ThirdPartyError::AuthExpired)?;
    let gw = gateway(&console.region, console.site);
    let usage: UsagePayload = call(&gw, console, USAGE_API)?;
    // The other two only enrich the bars — a failure there must not drop the
    // window percentages, which are the point of the fetch.
    let subscription: Option<SubscriptionPayload> = call(&gw, console, SUBSCRIPTION_API).ok();
    let quota: Option<QuotaConfig> = call(&gw, console, QUOTA_CONFIG_API).ok();
    Ok(stats(&usage, subscription.as_ref(), quota.as_ref()))
}

/// One gateway RPC: build the form body, POST it, unwrap the envelope.
fn call<T: DeserializeOwned>(
    gw: &Gateway,
    console: &ConsoleCredential,
    api: &str,
) -> Result<T, ThirdPartyError> {
    let url = format!(
        "{origin}/cli/api.json?action={action}&product=sfm_bailian&api={api}",
        origin = gw.origin,
        action = gw.action,
        api = percent_encode(api),
    );
    let body = format!(
        "params={}&region={}",
        percent_encode(&request_params(api)),
        percent_encode(&console.region),
    );
    let text = post_form(&url, &console.token, &body)?;
    unwrap_payload(&text)
}

/// The `params` JSON the gateway demands. `V` and `cornerstoneParam` are both
/// mandatory — omitting either answers `Bad Request` — and `consoleSite` is that
/// literal for every region and front alike (it is hardcoded upstream), so it is
/// deliberately NOT derived from [`ConsoleSite`], which names a different axis.
fn request_params(api: &str) -> String {
    serde_json::json!({
        "Api": api,
        "V": "1.0",
        "Data": {
            "cornerstoneParam": {
                "protocol": "V2",
                "console": "ONE_CONSOLE",
                "productCode": "p_efm",
                "switchUserType": 3,
                "consoleSite": "BAILIAN_ALIYUN",
            }
        }
    })
    .to_string()
}

/// Form-encoded POST with the console bearer. Mirrors [`super::get_json`]'s
/// error mapping (429 carries the server's `retry-after`; any other >=400 is a
/// flat `Status`) — the body-level verdict is [`unwrap_payload`]'s job, since
/// this endpoint answers 200 whether or not it worked.
fn post_form(url: &str, token: &str, body: &str) -> Result<String, ThirdPartyError> {
    let mut response = crate::usage::http_agent()
        .post(url)
        .header("Accept", "*/*")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Authorization", &format!("Bearer {token}"))
        .send(body)
        .map_err(|_| ThirdPartyError::Network)?;
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::usage::parse_retry_after);
        return Err(ThirdPartyError::RateLimited { retry_after });
    }
    if status >= 400 {
        return Err(ThirdPartyError::Status);
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|_| ThirdPartyError::Network)
}

/// Peel the OneConsole envelope down to the API's own payload.
///
/// Split from HTTP so every wire shape below is testable without a network. A
/// dead session is reported as [`ThirdPartyError::AuthExpired`] from the BODY —
/// the HTTP status is 200 for it, so a status-only reader would call it a
/// success and then fail to parse.
fn unwrap_payload<T: DeserializeOwned>(text: &str) -> Result<T, ThirdPartyError> {
    let env: Envelope = serde_json::from_str(text).map_err(|_| ThirdPartyError::Parse)?;
    let Some(outer) = env.data else {
        return Err(ThirdPartyError::Parse);
    };
    if LOGIN_ERROR_CODES.contains(&outer.error_code.as_str()) {
        return Err(ThirdPartyError::AuthExpired);
    }
    if !outer.success {
        return Err(ThirdPartyError::Status);
    }
    let inner = outer
        .data_v2
        .and_then(|v| v.data)
        .ok_or(ThirdPartyError::Parse)?;
    if !inner.success {
        return Err(ThirdPartyError::Status);
    }
    let payload = inner.data.ok_or(ThirdPartyError::Parse)?;
    serde_json::from_value(payload).map_err(|_| ThirdPartyError::Parse)
}

/// Pure payloads → bars + plan + rows, split from HTTP for testability.
///
/// The 5h pair is emitted only when the response actually carries it: it is
/// documented and every tier publishes a `five_hour` allowance, yet a real Solo
/// account returned the weekly pair alone even after spending, so a synthesised
/// 5h bar would be a claim clauth cannot make.
fn stats(
    usage: &UsagePayload,
    subscription: Option<&SubscriptionPayload>,
    quota: Option<&QuotaConfig>,
) -> ThirdPartyStats {
    let tier = subscription.map(|s| s.spec_code.as_str()).unwrap_or("");
    let mut bars = Vec::new();
    if let Some(bar) = window_bar(
        "5h",
        usage.per_5_hour_percentage,
        usage.per_5_hour_reset_time,
        quota.and_then(|q| q.allowance(tier, "five_hour")),
    ) {
        bars.push(bar);
    }
    if let Some(bar) = window_bar(
        "7d",
        usage.per_1_week_percentage,
        usage.per_1_week_reset_time,
        quota.and_then(|q| q.allowance(tier, "weekly")),
    ) {
        bars.push(bar);
    }

    let mut rows = Vec::new();
    if let Some(sub) = subscription {
        rows.push(StatRow {
            label: "subscription".to_string(),
            value: String::new(),
            kind: StatRowKind::Heading,
        });
        if !sub.status.is_empty() {
            // A non-VALID subscription is flagged rather than folded into
            // `is_available`: the account can still bill pay-as-you-go through
            // the same api key, so "can't call the API" would overclaim.
            let valid = sub.status.eq_ignore_ascii_case("VALID");
            rows.push(StatRow {
                label: "status".to_string(),
                value: sub.status.to_ascii_lowercase(),
                kind: if valid {
                    StatRowKind::Body
                } else {
                    StatRowKind::Danger
                },
            });
        }
        if let Some(days) = sub.remaining_days {
            rows.push(StatRow {
                label: "remaining".to_string(),
                value: format!("{days} day{}", if days == 1 { "" } else { "s" }),
                kind: StatRowKind::Body,
            });
        }
    }

    ThirdPartyStats {
        is_available: true,
        rows,
        bars,
        plan: subscription
            .map(|s| s.spec_code.clone())
            .filter(|p| !p.is_empty()),
        endpoint: None,
        best_effort: false,
    }
}

/// One window's bar, or `None` when the response omitted its percentage.
///
/// `pct` arrives as a RATIO in 0..1 (0.1284 = 12.84%), so it is scaled by 100
/// and clamped — a hand-back above 1.0 would otherwise render a bar many times
/// its own width.
fn window_bar(
    label: &str,
    ratio: Option<f64>,
    reset_ms: Option<i64>,
    total: Option<f64>,
) -> Option<UsageBar> {
    let pct = ratio
        .filter(|r| r.is_finite())
        .map(|r| r * 100.0)?
        .clamp(0.0, 100.0);
    Some(UsageBar {
        label: label.to_string(),
        pct,
        resets_at: reset_ms.map(|ms| epoch_secs_to_iso(ms / 1000)),
        // The response carries no absolute consumption — it is the tier's
        // allowance times the reported fraction, so it exists only when the
        // quota-config leg answered.
        used: total.map(|t| t * pct / 100.0),
        total,
    })
}

// ── Wire types ──────────────────────────────────────────────────────────────────

/// `{"code":"200","data":{…},"httpStatusCode":"200"}` — the transport envelope.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Option<GatewayResult>,
}

/// The gateway's own verdict. `errorCode` carries the dead-session code even
/// though `httpStatus` is 200.
#[derive(Debug, Deserialize)]
struct GatewayResult {
    #[serde(default)]
    success: bool,
    #[serde(rename = "errorCode", default)]
    error_code: String,
    #[serde(rename = "DataV2", default)]
    data_v2: Option<DataV2>,
}

#[derive(Debug, Deserialize)]
struct DataV2 {
    #[serde(default)]
    data: Option<ApiResult>,
}

/// The wrapped API's own result, one layer below the gateway's.
#[derive(Debug, Deserialize)]
struct ApiResult {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// `…/api/v2/usage`. Every field is optional: a real Solo account returns the
/// weekly pair alone.
#[derive(Debug, Default, Deserialize)]
struct UsagePayload {
    #[serde(rename = "per1WeekPercentage", default)]
    per_1_week_percentage: Option<f64>,
    #[serde(rename = "per1WeekResetTime", default)]
    per_1_week_reset_time: Option<i64>,
    #[serde(rename = "per5HourPercentage", default)]
    per_5_hour_percentage: Option<f64>,
    #[serde(rename = "per5HourResetTime", default)]
    per_5_hour_reset_time: Option<i64>,
}

/// `…/api/v2/subscription`.
#[derive(Debug, Default, Deserialize)]
struct SubscriptionPayload {
    /// Tier id: `lite` / `standard` / `pro` / `max`. Doubles as the key into
    /// [`QuotaConfig`].
    #[serde(rename = "specCode", default)]
    spec_code: String,
    #[serde(default)]
    status: String,
    #[serde(rename = "remainingDays", default)]
    remaining_days: Option<i64>,
}

/// `…/api/v2/quota-config` — `{"lite":{"five_hour":700,"weekly":2500}, …}` plus
/// an `addon_quota` row of a different shape, which the same map absorbs.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
struct QuotaConfig(HashMap<String, HashMap<String, f64>>);

impl QuotaConfig {
    /// The absolute allowance for one tier's window, or `None` when the tier is
    /// unknown (no subscription read) or the table doesn't publish that window.
    ///
    /// A tier this returns `None` for costs the bar its `used / total` and
    /// NOTHING else, deliberately and quietly. A missing row is an expected
    /// state rather than a defect: `specCode` admits `max` while the observed
    /// `quota-config` publishes lite/standard/pro plus `addon_quota`, so a real
    /// subscriber can sit on a tier with no row — failing the fetch over it
    /// would delete their windows to protect an enrichment. Never guess a
    /// neighbouring tier's numbers: a wrong ceiling reads as measured.
    ///
    /// A zero or negative row is treated the same way. It would otherwise
    /// render `0 / 0` with a `used: Some(0.0)`, which claims a measured zero.
    fn allowance(&self, tier: &str, window: &str) -> Option<f64> {
        self.0.get(tier)?.get(window).copied().filter(|v| *v > 0.0)
    }
}

#[cfg(test)]
#[path = "../../tests/inline/providers_alibaba.rs"]
mod tests;
