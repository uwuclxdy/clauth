//! Generic usage engine for unrecognised api-key providers.
//!
//! Derives the API origin from the profile's `base_url`, probes a small curated
//! set of usage-endpoint paths against that origin (same host only — the api_key
//! already authorises it for completions), and scans the first response that
//! isn't an error envelope for percentage-windows (→ bars), scalar balances
//! (→ text rows), and a plan/tier. The working endpoint is recorded on the
//! returned stats so the next tick reuses it (one request steady-state).
//!
//! The scanner is a pure recursive walk over [`serde_json::Value`] — no
//! per-provider code. Bars win: a percentage-bearing object becomes a bar; only
//! when no bars are found do we harvest scalar balances into rows.

use serde_json::Value;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, UsageBar, api_origin};

/// Ordered candidate usage-endpoint paths, probed against the provider origin.
/// First whose response yields usage data wins. Curated from known providers
/// (z.ai, Anthropic-compatible, DeepSeek, OpenAI-style) plus generic guesses.
const CANDIDATE_PATHS: &[&str] = &[
    "/api/monitor/usage/quota/limit", // z.ai
    "/api/oauth/usage",               // Anthropic-compatible proxies
    "/user/balance",                  // DeepSeek
    "/v1/credits",                    // OpenAI-style prepaid credits
    "/api/credits",
    "/api/usage",
    "/usage",
];

/// Fetch generic usage. `hint` is the endpoint path that last worked (read from
/// the in-memory store by the caller); tried first so steady state is one
/// request. Probes candidates until one yields data; a host-level 429 propagates
/// immediately so the caller can defer the profile.
pub(super) fn fetch(
    base_url: &str,
    api_key: &str,
    hint: Option<&str>,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    let origin = api_origin(base_url).ok_or(ThirdPartyError::Network)?;

    // Hint first, then the curated list (deduped).
    let mut ordered: Vec<&str> = Vec::with_capacity(CANDIDATE_PATHS.len() + 1);
    if let Some(h) = hint.filter(|h| !h.is_empty()) {
        ordered.push(h);
    }
    for p in CANDIDATE_PATHS {
        if !ordered.contains(p) {
            ordered.push(p);
        }
    }

    for path in ordered {
        let url = format!("{origin}{path}");
        let text = match super::get_json(&url, api_key) {
            Ok(t) => t,
            Err(ThirdPartyError::RateLimited { retry_after }) => {
                // Host-level throttle: stop probing, let the caller defer.
                return Err(ThirdPartyError::RateLimited { retry_after });
            }
            Err(ThirdPartyError::AuthExpired) => {
                // Only a HINT 401 proves the key died: that endpoint worked
                // before, so its verdict is about the credential, not the
                // route. A candidate path is a guess — hosts that 401
                // unmatched routes exist, and a false verdict here writes a
                // durable record a key re-entry cannot clear (the fingerprint
                // hashes the key, so re-entering the SAME key matches the
                // record again). A candidate 401 is just another miss.
                if Some(path) == hint {
                    return Err(ThirdPartyError::AuthExpired);
                }
                continue;
            }
            Err(_) => continue, // 404 / network / parse — try the next candidate
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if is_error_envelope(&value) {
            continue;
        }
        let (plan, bars, rows) = scan(&value);
        if bars.is_empty() && rows.is_empty() && plan.is_none() {
            continue; // parsed, but carries no usage signal
        }
        return Ok(ThirdPartyStats {
            is_available: true,
            rows,
            bars,
            plan,
            endpoint: Some(path.to_string()),
            // Unknown provider, mapped heuristically — flag it so the UI can hint
            // "looks wrong? open an issue".
            best_effort: true,
        });
    }
    // No candidate yielded usable data this pass — surface as a generic failure
    // so the caller falls back to cache / shows the no-usage state.
    Err(ThirdPartyError::Status)
}

/// True when `value` is a provider error envelope dressed as HTTP 200 (z.ai
/// returns `{"code":500,"msg":"404 NOT_FOUND","success":false}` for unknown
/// routes). Such a body must never count as empty-but-valid usage.
fn is_error_envelope(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.get("success").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    obj.get("code")
        .and_then(Value::as_i64)
        .is_some_and(|c| c >= 400)
        || obj
            .get("status")
            .and_then(Value::as_i64)
            .is_some_and(|s| s >= 400)
}

/// Walk an arbitrary JSON value and extract `(plan, bars, rows)`. Bars take
/// priority: a percentage-bearing object becomes a bar; scalar balances are
/// harvested into rows only when no bars were found. Plan is independent.
fn scan(value: &Value) -> (Option<String>, Vec<UsageBar>, Vec<StatRow>) {
    let mut plan = None;
    let mut bars: Vec<UsageBar> = Vec::new();
    scan_inner(value, &mut plan, &mut bars);

    let rows = if bars.is_empty() {
        let mut rows = Vec::new();
        harvest_scalars(value, &mut rows);
        dedup_rows(&mut rows);
        rows
    } else {
        Vec::new()
    };
    (plan, bars, rows)
}

fn scan_inner(value: &Value, plan: &mut Option<String>, bars: &mut Vec<UsageBar>) {
    match value {
        Value::Object(obj) => {
            if plan.is_none()
                && let Some(p) = find_plan(obj)
            {
                *plan = Some(p);
            }
            if let Some(bar) = extract_bar(obj) {
                bars.push(bar);
            }
            for v in obj.values() {
                scan_inner(v, plan, bars);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                scan_inner(v, plan, bars);
            }
        }
        _ => {}
    }
}

fn find_plan(obj: &serde_json::Map<String, Value>) -> Option<String> {
    obj.iter().find_map(|(k, v)| {
        if is_plan_key(k)
            && let Some(s) = v.as_str()
        {
            let s = s.trim();
            (!s.is_empty() && s.len() <= 32).then(|| s.to_string())
        } else {
            None
        }
    })
}

/// A bar is an object carrying a percentage-like field in 0..=100, optionally a
/// sibling reset timestamp, a label field, and absolute used/total amounts.
fn extract_bar(obj: &serde_json::Map<String, Value>) -> Option<UsageBar> {
    let pct = obj.iter().find_map(|(k, v)| {
        is_pct_key(k)
            .then(|| v.as_f64())
            .flatten()
            .filter(|&p| (0.0..=100.0).contains(&p))
    })?;
    let resets_at = obj
        .iter()
        .find_map(|(k, v)| is_reset_key(k).then(|| parse_reset(v)).flatten());
    let label = obj
        .iter()
        .find_map(|(k, v)| {
            is_label_key(k)
                .then(|| v.as_str())
                .flatten()
                .map(humanize_label)
        })
        .unwrap_or_else(|| "usage".to_string());
    // Absolute amounts. `total` prefers an explicit ceiling field; when the
    // object only carries `used` + `remaining` (z.ai), `used + remaining` is the
    // robust fallback so the bar still shows `x / y`.
    let used = num_field(obj, is_used_key);
    let total = num_field(obj, is_total_key)
        .or_else(|| used.and_then(|u| num_field(obj, is_remaining_key).map(|r| u + r)));
    Some(UsageBar {
        label,
        pct,
        resets_at,
        used,
        total,
    })
}

/// First numeric value under a key matching `pred`, accepting ints, floats, and
/// numeric strings (`"10.50"`).
fn num_field(obj: &serde_json::Map<String, Value>, pred: fn(&str) -> bool) -> Option<f64> {
    obj.iter()
        .find_map(|(k, v)| pred(k).then(|| as_f64_loose(v)).flatten())
}

fn as_f64_loose(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn harvest_scalars(value: &Value, rows: &mut Vec<StatRow>) {
    match value {
        Value::Object(obj) => {
            for (k, v) in obj.iter() {
                if let Some(num) = scalar_value(k, v) {
                    rows.push(StatRow {
                        label: humanize_label(k),
                        value: num,
                        kind: StatRowKind::Body,
                    });
                }
            }
            for v in obj.values() {
                harvest_scalars(v, rows);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                harvest_scalars(v, rows);
            }
        }
        _ => {}
    }
}

fn scalar_value(key: &str, value: &Value) -> Option<String> {
    if !is_scalar_key(key) {
        return None;
    }
    if let Some(n) = value.as_f64() {
        return Some(format_number(n));
    }
    // Some providers return balances as numeric strings ("10.50").
    if let Some(s) = value.as_str()
        && let Ok(n) = s.trim().parse::<f64>()
    {
        return Some(format_number(n));
    }
    None
}

fn dedup_rows(rows: &mut Vec<StatRow>) {
    let mut seen: Vec<(String, String)> = Vec::with_capacity(rows.len());
    rows.retain(|r| {
        let key = (r.label.clone(), r.value.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

/// Parse a reset value: epoch-ms int (z.ai `nextResetTime`), epoch-secs int, or
/// an ISO-8601 string — normalised to the ISO form clauth renders.
fn parse_reset(value: &Value) -> Option<String> {
    if let Some(n) = value.as_i64() {
        // Heuristic: values past 10^12 (year 2001 as ms — below any real future
        // reset) are milliseconds; smaller values are seconds.
        let secs = if n > 1_000_000_000_000 { n / 1000 } else { n };
        return Some(crate::usage::epoch_secs_to_iso(secs));
    }
    if let Some(s) = value.as_str()
        && let Some(secs) = crate::usage::iso_to_epoch_secs(s)
    {
        return Some(crate::usage::epoch_secs_to_iso(secs));
    }
    None
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.2}")
    }
}

/// Split camelCase / snake_case / kebab-case into lowercase words. `TIME_LIMIT`
/// → "time limit", `modelCode` → "model code". All-caps runs stay one word.
fn humanize_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch == '_' || ch == '-' || ch == ' ' {
            out.push(' ');
            prev_lower = false;
            continue;
        }
        let is_lower = ch.is_lowercase();
        if ch.is_uppercase() && prev_lower {
            out.push(' ');
        }
        out.extend(ch.to_lowercase());
        prev_lower = is_lower || ch.is_ascii_digit();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_plan_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "level"
            | "plan"
            | "tier"
            | "subscription"
            | "subscription_type"
            | "organization_type"
            | "account_type"
    )
}

fn is_pct_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "percentage"
            | "percent"
            | "pct"
            | "utilization"
            | "utilization_pct"
            | "used_percent"
            | "usedpercent"
            | "used_pct"
    )
}

fn is_reset_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "nextresettime"
            | "resets_at"
            | "reset_at"
            | "resettime"
            | "reset_time"
            | "reset_date"
            | "expiry"
            | "expires"
            | "expires_at"
            | "expirestime"
    )
}

fn is_label_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "type"
            | "name"
            | "modelcode"
            | "model"
            | "label"
            | "title"
            | "scope"
            | "period"
            | "category"
    )
}

fn is_scalar_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "balance"
            | "balances"
            | "total"
            | "total_balance"
            | "granted_balance"
            | "topped_up_balance"
            | "credit"
            | "credits"
            | "remaining"
            | "left"
            | "usage"
            | "used"
            | "currentvalue"
            | "tokens"
            | "total_tokens"
            | "cost"
            | "amount"
            | "quota"
    )
}

/// Consumed amount paired with a bar's percentage (the `x` of `x / y`).
fn is_used_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "currentvalue" | "current_value" | "used" | "consumed" | "spent" | "usedamount"
    )
}

/// Explicit window ceiling (the `y` of `x / y`). `usage` is deliberately
/// excluded — it is provider-ambiguous (z.ai means the ceiling, others the
/// consumed amount); the `used + remaining` fallback covers the z.ai shape.
fn is_total_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "total" | "limit" | "quota" | "max" | "capacity" | "budget" | "allowance" | "ceiling"
    )
}

fn is_remaining_key(k: &str) -> bool {
    matches!(k.to_ascii_lowercase().as_str(), "remaining" | "left")
}

#[cfg(test)]
#[path = "../../tests/inline/providers_generic.rs"]
mod tests;
