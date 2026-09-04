//! MiniMax provider — Token Plan interval + weekly windows.
//!
//! One endpoint: `GET https://api.minimax.io/v1/token_plan/remains`, authorised
//! by the same api key the `/anthropic` completions endpoint takes. It answers
//! `model_remains[]` — one entry per plan bucket (`general` for text, `video`
//! for the video product), each carrying a rolling interval window and a weekly
//! one as REMAINING percentages plus their end instants in epoch-ms.
//!
//! Claude Code traffic bills against the `general` bucket, so that entry drives
//! the bars; every bucket is listed as a text row, since a reader picking this
//! account wants to see the whole plan. `video` shares the account but not the
//! window Claude Code spends, which is exactly why it must not become a bar.
//!
//! Only the international host is claimed. MiniMax also fronts a China-region
//! endpoint, but the typed fetch resolves its quota host from a constant rather
//! than from the profile's own `base_url` (see [`ThirdPartyTarget`]), so
//! claiming a second origin here would send one region's api key to the other
//! region's host. A CN account keeps falling through to the generic scanner
//! until that plumbing carries the base URL.
//!
//! [`ThirdPartyTarget`]: super::ThirdPartyTarget

use serde::Deserialize;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, UsageBar};
use crate::usage::{LABEL_5H, LABEL_7D, epoch_secs_to_iso};

pub(super) const DISPLAY_NAME: &str = "MiniMax";

pub(super) const ORIGIN: &str = "https://api.minimax.io";

/// Where an operator mints the api key this provider authenticates with, as
/// published by <https://platform.minimax.io/docs/guides/text-generation>.
pub(super) const CONSOLE_URL: &str = "https://platform.minimax.io/user-center/basic-information";

const REMAINS_PATH: &str = "/v1/token_plan/remains";

/// The plan bucket Claude Code's completions bill against — the one whose
/// windows become bars. Every other bucket (`video`) rides as a row only.
const CLAUDE_CODE_BUCKET: &str = "general";

pub(super) fn matches_base_url(url: &str) -> bool {
    super::url_matches_host(url, ORIGIN)
}

pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let text = super::get_json(&format!("{ORIGIN}{REMAINS_PATH}"), api_key)?;
    let body: RemainsResponse = serde_json::from_str(&text).map_err(|_| ThirdPartyError::Parse)?;
    // `base_resp.status_code` is MiniMax's own verdict and rides an HTTP 200:
    // a rejected key answers 200 with a non-zero code, so the transport status
    // alone would publish an empty plan as a healthy one.
    if body.base_resp.status_code != 0 {
        return Err(ThirdPartyError::Status);
    }
    Ok(stats(&body.model_remains))
}

/// Pure `model_remains[]` → bars + rows, split from HTTP for testability.
fn stats(models: &[ModelRemains]) -> ThirdPartyStats {
    ThirdPartyStats {
        is_available: true,
        rows: rows(models),
        bars: bars(models),
        // The response names no plan tier — the endpoint path is the only
        // "token plan" in it, and inventing a label from a path would put a
        // word in the vendor's mouth on every account.
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

/// The bucket whose windows become bars: `general` when the account has one,
/// else a lone bucket (an account entitled to exactly one product cannot be
/// ambiguous). An account carrying several buckets and no `general` yields no
/// bars rather than a guess about which one Claude Code spends.
fn bar_bucket(models: &[ModelRemains]) -> Option<&ModelRemains> {
    models
        .iter()
        .find(|m| m.model_name == CLAUDE_CODE_BUCKET)
        .or(match models {
            [only] => Some(only),
            _ => None,
        })
}

/// 5h + 7d bars off the Claude Code bucket. MiniMax reports what is LEFT, so
/// each percentage is inverted into the utilization every other provider's bar
/// carries; a bucket missing one of the two contributes only the other.
fn bars(models: &[ModelRemains]) -> Vec<UsageBar> {
    let Some(m) = bar_bucket(models) else {
        return Vec::new();
    };
    let mut bars = Vec::new();
    if let Some(pct) = utilization(m.current_interval_remaining_percent) {
        bars.push(UsageBar {
            label: LABEL_5H.to_string(),
            pct,
            resets_at: m.end_time.map(ms_to_iso),
            used: None,
            total: None,
        });
    }
    if let Some(pct) = utilization(m.current_weekly_remaining_percent) {
        bars.push(UsageBar {
            label: LABEL_7D.to_string(),
            pct,
            resets_at: m.weekly_end_time.map(ms_to_iso),
            used: None,
            total: None,
        });
    }
    bars
}

/// One row per plan bucket, so an account's whole entitlement is visible even
/// though only [`CLAUDE_CODE_BUCKET`] draws bars. The row states what is LEFT,
/// matching the vendor's own console wording rather than the bars' inverted
/// figure — the label says `remaining`, so the two never read as contradicting
/// each other.
fn rows(models: &[ModelRemains]) -> Vec<StatRow> {
    if models.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![StatRow {
        label: "remaining".to_string(),
        value: String::new(),
        kind: StatRowKind::Heading,
    }];
    for m in models {
        let pct = |p: Option<i64>| match p {
            Some(p) => format!("{p}%"),
            None => "-".to_string(),
        };
        rows.push(StatRow {
            label: m.model_name.clone(),
            value: format!(
                "{LABEL_5H} {}  ·  {LABEL_7D} {}",
                pct(m.current_interval_remaining_percent),
                pct(m.current_weekly_remaining_percent)
            ),
            // A spent bucket is the one figure a reader is scanning for.
            kind: match m.current_interval_remaining_percent {
                Some(0) => StatRowKind::Danger,
                _ => StatRowKind::Body,
            },
        });
    }
    rows
}

/// Remaining percentage → utilization, clamped into 0..=100. `None` for an
/// absent figure, so a bucket reporting nothing yields no bar rather than a
/// full-headroom one.
fn utilization(remaining_pct: Option<i64>) -> Option<f64> {
    remaining_pct.map(|r| (100.0 - r as f64).clamp(0.0, 100.0))
}

/// Epoch-ms (MiniMax `end_time` / `weekly_end_time`) → ISO-8601 UTC.
fn ms_to_iso(ms: i64) -> String {
    epoch_secs_to_iso(ms / 1000)
}

// ── Wire types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct RemainsResponse {
    #[serde(default)]
    model_remains: Vec<ModelRemains>,
    #[serde(default)]
    base_resp: BaseResp,
}

/// MiniMax's in-band result envelope: `status_code` 0 is success, and it rides
/// an HTTP 200 even for a rejected key.
#[derive(Debug, Default, Deserialize)]
struct BaseResp {
    #[serde(default)]
    status_code: i64,
}

#[derive(Debug, Default, Deserialize)]
struct ModelRemains {
    #[serde(default)]
    model_name: String,
    /// End of the rolling interval window, epoch-ms.
    #[serde(default)]
    end_time: Option<i64>,
    /// End of the weekly window, epoch-ms.
    #[serde(default)]
    weekly_end_time: Option<i64>,
    /// Percentage of the interval window still available (100 = untouched).
    #[serde(default)]
    current_interval_remaining_percent: Option<i64>,
    /// Percentage of the weekly window still available.
    #[serde(default)]
    current_weekly_remaining_percent: Option<i64>,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_minimax.rs"]
mod tests;
