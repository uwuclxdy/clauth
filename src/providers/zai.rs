//! Z.ai provider — usage limits + per-model token totals.
//!
//! Two monitor endpoints under `https://api.z.ai/api/monitor/usage`:
//! - `/quota/limit` (no params): `data.limits[]` percentage windows → bars, each
//!   with absolute `currentValue`/`remaining` and an optional per-web-tool
//!   `usageDetails` breakdown; `data.level` → plan.
//! - `/model-usage?startTime=&endTime=` (`yyyy-MM-dd HH:mm:ss`, UTC): a 7-day
//!   window's `totalUsage.modelSummaryList[]` per-model token totals → text rows.
//!   Best-effort — a failure never drops the limit bars.

use serde::Deserialize;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, UsageBar};
use crate::usage::epoch_secs_to_iso;

pub(super) const DISPLAY_NAME: &str = "Z.ai";

pub(super) const ORIGIN: &str = "https://api.z.ai";

/// Where an operator mints the api key this provider authenticates with, as
/// published by <https://docs.z.ai/guides/overview/quick-start>.
pub(super) const CONSOLE_URL: &str = "https://z.ai/manage-apikey/apikey-list";

const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const MODEL_USAGE_PATH: &str = "/api/monitor/usage/model-usage";
/// Window for the model-usage breakdown, in seconds (7 days).
const MODEL_USAGE_WINDOW_SECS: i64 = 7 * 86_400;

pub(super) fn matches_base_url(url: &str) -> bool {
    super::url_matches_host(url, ORIGIN)
}

pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let quota_text = super::get_json(&format!("{ORIGIN}{QUOTA_PATH}"), api_key)?;
    let quota: ZaiEnvelope<QuotaData> =
        serde_json::from_str(&quota_text).map_err(|_| ThirdPartyError::Parse)?;
    if !quota.success {
        return Err(ThirdPartyError::Status);
    }
    let mut stats = quota_stats(&quota.data);

    // Model-token breakdown: best-effort, appended as rows. A failure (or empty
    // window) leaves the limit bars intact.
    let now = crate::usage::now_epoch_secs();
    if let Some(rows) = fetch_model_rows(api_key, now - MODEL_USAGE_WINDOW_SECS, now) {
        stats.rows.extend(rows);
    }
    Ok(stats)
}

/// Pure `quota/limit` → bars + plan + per-tool detail rows, split from HTTP for
/// testability.
fn quota_stats(data: &QuotaData) -> ThirdPartyStats {
    let mut bars: Vec<UsageBar> = Vec::new();
    let mut detail_rows: Vec<StatRow> = Vec::new();
    // Shortest window first (5h → 7d → 30d). Limits with an undecodable window
    // sort last; the stable sort keeps source order within a tier.
    let mut ordered: Vec<&Limit> = data.limits.iter().collect();
    ordered.sort_by_key(|l| window_secs(l.unit, l.number).unwrap_or(i64::MAX));
    for limit in ordered {
        let pct = limit.percentage.clamp(0.0, 100.0);
        // `currentValue` (used) + `remaining` give the absolute window; the
        // ambiguously-named `usage` field equals the same total, used as fallback.
        let used = limit.current_value;
        let total = match (limit.current_value, limit.remaining) {
            (Some(c), Some(r)) => Some(c + r),
            _ => limit.usage,
        };
        bars.push(UsageBar {
            label: limit_label(limit),
            pct,
            resets_at: limit.next_reset_time.map(ms_to_iso),
            used,
            total,
        });
        // Per-web-tool breakdown (search-prime / web-reader / zread), only when
        // anything has been used — an all-zero breakdown is noise.
        if limit.usage_details.iter().any(|d| d.usage > 0.0) {
            detail_rows.push(StatRow {
                label: limit_label(limit),
                value: String::new(),
                kind: StatRowKind::Heading,
            });
            for d in &limit.usage_details {
                detail_rows.push(StatRow {
                    label: d.model_code.clone(),
                    value: fmt_count(d.usage),
                    kind: StatRowKind::Body,
                });
            }
        }
    }
    ThirdPartyStats {
        is_available: true,
        rows: detail_rows,
        bars,
        plan: data.level.clone(),
        endpoint: None,
        best_effort: false,
    }
}

/// Fetch the model-usage window and map it to per-model token rows. `None` on
/// any failure or an empty window — the caller keeps the limit bars regardless.
fn fetch_model_rows(api_key: &str, start_secs: i64, end_secs: i64) -> Option<Vec<StatRow>> {
    let url = format!(
        "{ORIGIN}{MODEL_USAGE_PATH}?startTime={}&endTime={}",
        url_encode(&secs_to_zai_date(start_secs)),
        url_encode(&secs_to_zai_date(end_secs)),
    );
    let text = super::get_json(&url, api_key).ok()?;
    let env: ZaiEnvelope<ModelUsageData> = serde_json::from_str(&text).ok()?;
    if !env.success {
        return None;
    }
    Some(model_rows(&env.data.total_usage))
}

/// Pure `model-usage.totalUsage` → token rows. Empty when no model recorded any
/// tokens (so the caller appends nothing).
fn model_rows(total: &ModelTotalUsage) -> Vec<StatRow> {
    if total
        .model_summary_list
        .iter()
        .all(|m| m.total_tokens <= 0.0)
    {
        return Vec::new();
    }
    let mut rows = vec![StatRow {
        label: "7d tokens".to_string(),
        value: String::new(),
        kind: StatRowKind::Heading,
    }];
    for m in &total.model_summary_list {
        rows.push(StatRow {
            label: m.model_name.clone(),
            value: fmt_count(m.total_tokens),
            kind: StatRowKind::Body,
        });
    }
    rows.push(StatRow {
        label: "total".to_string(),
        value: format!(
            "{}  ({} calls)",
            fmt_count(total.total_tokens_usage),
            fmt_count(total.total_model_call_count)
        ),
        kind: StatRowKind::Faint,
    });
    rows
}

/// Bar label for a limit. Prefers the window decoded from `unit`/`number`
/// (`5h`, `7d`, `30d`) since GLM's token limits are per-window caps the user
/// thinks of in those terms; falls back to the API type (`tokens limit`) when
/// the window code is unknown. The two token limits thus read `5h` / `7d`
/// instead of a duplicated `tokens limit`.
fn limit_label(limit: &Limit) -> String {
    window_label(limit.unit, limit.number).unwrap_or_else(|| type_label(&limit.limit_type))
}

/// `TIME_LIMIT` → "time limit", `TOKENS_LIMIT` → "tokens limit", any other type
/// lowercased with `_` → space.
fn type_label(limit_type: &str) -> String {
    limit_type.to_ascii_lowercase().replace('_', " ")
}

/// Window length in seconds from z.ai's duration code `unit` × `number`. `unit`
/// is a calendar code (3 = hour, 6 = week, 5 = month) carried on every limit —
/// present even when `nextResetTime` is absent, which makes it the reliable
/// source for both the label and the bar ordering. `None` for an unknown code.
fn window_secs(unit: Option<i64>, number: Option<i64>) -> Option<i64> {
    let n = number.filter(|&n| n > 0)?;
    let unit_secs = match unit? {
        3 => 3600,        // hour
        6 => 7 * 86_400,  // week
        5 => 30 * 86_400, // month (nominal)
        _ => return None,
    };
    Some(n * unit_secs)
}

/// Compact window label (`5h`, `7d`, `30d`) from the decoded [`window_secs`], or
/// `None` for an unknown code so the caller falls back to the API type label.
fn window_label(unit: Option<i64>, number: Option<i64>) -> Option<String> {
    let secs = window_secs(unit, number)?;
    Some(if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    })
}

/// Compact token/count formatting: `52245057` → `52.2M`, `44456531` → `44.5M`,
/// `451` → `451`. Negative/NaN clamps to `0`.
fn fmt_count(n: f64) -> String {
    if !n.is_finite() || n <= 0.0 {
        return "0".to_string();
    }
    if n >= 1e9 {
        format!("{:.1}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}k", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

/// Epoch-ms (z.ai `nextResetTime`) → ISO-8601 UTC.
fn ms_to_iso(ms: i64) -> String {
    epoch_secs_to_iso(ms / 1000)
}

/// Epoch-secs → z.ai's `yyyy-MM-dd HH:mm:ss` UTC date param (space-separated, no
/// timezone), derived from the ISO form clauth already emits.
fn secs_to_zai_date(secs: i64) -> String {
    let iso = epoch_secs_to_iso(secs); // YYYY-MM-DDTHH:MM:SS+00:00
    let date = iso.get(..19).unwrap_or(&iso);
    date.replacen('T', " ", 1)
}

/// Minimal percent-encoding for the date param: space → `%20`, colon → `%3A`.
/// Sufficient for `yyyy-MM-dd HH:mm:ss`; not a general-purpose encoder.
fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            ':' => "%3A".to_string(),
            other => other.to_string(),
        })
        .collect()
}

// ── Wire types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ZaiEnvelope<T: Default> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: T,
}

#[derive(Debug, Default, Deserialize)]
struct QuotaData {
    #[serde(default)]
    limits: Vec<Limit>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Limit {
    #[serde(rename = "type", default)]
    limit_type: String,
    /// Window duration code: 3 = hour, 6 = week, 5 = month (the codes z.ai uses).
    /// Paired with `number` to give the window length — present even when
    /// `nextResetTime` is absent, so it's the reliable mapping source.
    #[serde(default)]
    unit: Option<i64>,
    /// Multiplier on `unit` — `number: 5` + `unit: 3` (hour) = a 5-hour window.
    #[serde(default)]
    number: Option<i64>,
    #[serde(default)]
    percentage: f64,
    #[serde(rename = "nextResetTime", default)]
    next_reset_time: Option<i64>,
    #[serde(default)]
    usage: Option<f64>,
    #[serde(rename = "currentValue", default)]
    current_value: Option<f64>,
    #[serde(default)]
    remaining: Option<f64>,
    #[serde(rename = "usageDetails", default)]
    usage_details: Vec<UsageDetail>,
}

#[derive(Debug, Deserialize)]
struct UsageDetail {
    #[serde(rename = "modelCode", default)]
    model_code: String,
    #[serde(default)]
    usage: f64,
}

#[derive(Debug, Default, Deserialize)]
struct ModelUsageData {
    #[serde(rename = "totalUsage", default)]
    total_usage: ModelTotalUsage,
}

#[derive(Debug, Default, Deserialize)]
struct ModelTotalUsage {
    #[serde(rename = "totalModelCallCount", default)]
    total_model_call_count: f64,
    #[serde(rename = "totalTokensUsage", default)]
    total_tokens_usage: f64,
    #[serde(rename = "modelSummaryList", default)]
    model_summary_list: Vec<ModelSummary>,
}

#[derive(Debug, Deserialize)]
struct ModelSummary {
    #[serde(rename = "modelName", default)]
    model_name: String,
    #[serde(rename = "totalTokens", default)]
    total_tokens: f64,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_zai.rs"]
mod tests;
