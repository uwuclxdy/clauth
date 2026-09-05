//! OpenRouter provider — credit stats from two endpoints.
//!
//! `/api/v1/credits` is the wallet: `total_credits` purchased vs `total_usage`
//! spent. The remaining balance is their difference, NEGATIVE once the account
//! is overdrawn — the state an inference call answers with `402 ... can only
//! afford 0` (measured 2026-08-17). It answers for a REGULAR api key: the
//! docs' "management key required" note is stale, verified on two regular keys
//! the same day.
//!
//! `/api/v1/key` is best-effort enrichment: the daily/weekly/monthly usage, the
//! free-tier flag, and the key's own cap (`limit` / `limit_remaining`). A `null`
//! cap says the key has no cap of its own — NOTHING about the wallet, which a
//! null-cap key can still overdraw (the same probe). The key's `usage` field is
//! per-key (since minting), not the account's, so the wallet never reads it.
//!
//! Wire shapes per <https://openrouter.ai/docs/api_reference/limits> and
//! <https://openrouter.ai/docs/api/api-reference/credits/get-remaining-credits>.
//! `label`, `byok_*`, `is_management_key` and the deprecated `rate_limit`
//! object are deliberately not modeled — nothing renders them.

use serde::Deserialize;

use super::{
    DEEPSEEK_BALANCE_ROW_LABEL, StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats,
    url_matches_host,
};

pub(super) const DISPLAY_NAME: &str = "OpenRouter";

pub(super) const ORIGIN: &str = "https://openrouter.ai";

const CREDITS_PATH: &str = "/api/v1/credits";
const KEY_PATH: &str = "/api/v1/key";

/// Where an operator mints the api key this provider authenticates with, as
/// published by <https://openrouter.ai/docs/quickstart> ("Your first request").
pub(super) const CONSOLE_URL: &str = "https://openrouter.ai/settings/keys";

pub(super) fn matches_base_url(url: &str) -> bool {
    url_matches_host(url, ORIGIN)
}

pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let credits_text = super::get_json(&format!("{ORIGIN}{CREDITS_PATH}"), api_key)?;
    let credits: CreditsEnvelope =
        serde_json::from_str(&credits_text).map_err(|_| ThirdPartyError::Parse)?;
    // Key stats: best-effort enrichment. A failure (or an empty response)
    // drops the period rows and the free-tier flag, never the wallet.
    let key = super::get_json(&format!("{ORIGIN}{KEY_PATH}"), api_key)
        .ok()
        .and_then(|text| serde_json::from_str::<KeyEnvelope>(&text).ok());
    Ok(stats(&credits.data, key.as_ref().map(|k| &k.data)))
}

/// Pure responses → display rows, separated from HTTP for testability. `key` is
/// the best-effort half; `None` drops the period rows and the free-tier flag,
/// never the wallet.
fn stats(credits: &CreditsData, key: Option<&KeyData>) -> ThirdPartyStats {
    let remaining = credits.total_credits - credits.total_usage;
    let mut rows = vec![StatRow {
        label: "credits".to_string(),
        value: String::new(),
        kind: StatRowKind::Heading,
    }];
    // The remaining-credits row shares the wallet label on purpose: the MCP
    // roster's balance rank and the overview's balance column both single that
    // label out, and a remaining credit pool is the spendable balance.
    rows.push(StatRow {
        label: DEEPSEEK_BALANCE_ROW_LABEL.to_string(),
        value: dollars(remaining),
        // Danger must agree with what the row SAYS: anything under half a cent
        // (an overdrawn account included) renders as `0.00 USD` or worse, so an
        // exact `== 0.0` test would leave a spent key reading as a healthy one.
        kind: if remaining < 0.005 {
            StatRowKind::Danger
        } else {
            StatRowKind::Body
        },
    });
    rows.push(StatRow {
        label: "used".to_string(),
        value: dollars(credits.total_usage),
        kind: StatRowKind::Body,
    });
    rows.push(StatRow {
        label: "purchased".to_string(),
        value: dollars(credits.total_credits),
        kind: StatRowKind::Body,
    });
    if let Some(key) = key {
        rows.push(StatRow {
            label: "today".to_string(),
            value: dollars(key.usage_daily),
            kind: StatRowKind::Body,
        });
        rows.push(StatRow {
            label: "this week".to_string(),
            value: dollars(key.usage_weekly),
            kind: StatRowKind::Body,
        });
        rows.push(StatRow {
            label: "this month".to_string(),
            value: dollars(key.usage_monthly),
            kind: StatRowKind::Body,
        });
        if let Some(cap) = key.limit {
            rows.push(StatRow {
                label: "key limit".to_string(),
                value: dollars(cap),
                kind: StatRowKind::Body,
            });
        }
        if let Some(left) = key.limit_remaining {
            rows.push(StatRow {
                label: "key limit left".to_string(),
                value: dollars(left),
                kind: StatRowKind::Body,
            });
        }
        if key.is_free_tier {
            rows.push(StatRow {
                label: "free tier".to_string(),
                value: String::new(),
                kind: StatRowKind::Faint,
            });
        }
    }
    if remaining >= 0.005 {
        ThirdPartyStats::from_rows(rows)
    } else {
        // An overdrawn wallet cannot afford any call, so the daemon's
        // reachability dot must read red. The rows still render, and
        // `unfunded` appends the shared refusal beside them.
        ThirdPartyStats::unfunded(rows)
    }
}

/// `1.5` → `"1.50 USD"` — the `amount currency` shape `parse_balance` reads (a `$` prefix would parse as no wallet). An overdrawn remaining formats negative (`-0.20 USD`), which the funded-wallet selection drops, so the roster ranks the account `Unknown`.
fn dollars(n: f64) -> String {
    format!("{n:.2} USD")
}

// ── Wire types ──────────────────────────────────────────────────────────────────

/// `total_credits` and `total_usage` are required numbers — a body missing
/// either fails the parse rather than inventing a wallet.
#[derive(Debug, Clone, Deserialize)]
struct CreditsEnvelope {
    data: CreditsData,
}

#[derive(Debug, Clone, Deserialize)]
struct CreditsData {
    total_credits: f64,
    total_usage: f64,
}

/// `data` is required — an error envelope carrying no key info must never read
/// as usable usage — but its fields default: a degraded body renders zeros
/// rather than dropping the whole wallet, matching z.ai's `Default`-bound
/// envelope. The wire is documented and current, so the leniency is deliberate
/// rather than a shape guess.
#[derive(Debug, Clone, Deserialize)]
struct KeyEnvelope {
    data: KeyData,
}

#[derive(Debug, Clone, Deserialize)]
struct KeyData {
    /// The key's own spending cap, `null` when the key carries none. Says
    /// nothing about the wallet — the credits endpoint is the wallet.
    #[serde(default)]
    limit: Option<f64>,
    /// What is left under the key's own cap, `null` with `limit`.
    #[serde(default)]
    limit_remaining: Option<f64>,
    /// Credits used in the current UTC day.
    #[serde(default)]
    usage_daily: f64,
    /// Credits used in the current UTC week.
    #[serde(default)]
    usage_weekly: f64,
    /// Credits used in the current UTC month.
    #[serde(default)]
    usage_monthly: f64,
    /// Whether the user has ever paid for credits.
    #[serde(default)]
    is_free_tier: bool,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_openrouter.rs"]
mod tests;
