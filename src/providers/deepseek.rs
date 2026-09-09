//! DeepSeek provider — balance stats from `GET /user/balance`.
//!
//! Wire shape per <https://api-docs.deepseek.com/api/get-user-balance>.

use serde::Deserialize;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, url_matches_host};

pub(super) const DISPLAY_NAME: &str = "DeepSeek";

pub(super) const ORIGIN: &str = "https://api.deepseek.com";
const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// Where an operator mints the api key this provider authenticates with, as
/// published by <https://api-docs.deepseek.com/> ("Your First API Call").
pub(super) const CONSOLE_URL: &str = "https://platform.deepseek.com/api_keys";

/// Label of the row carrying `total_balance` — the amount still spendable, not a
/// sum of the two rows under it, which is what `total` beside `granted` and
/// `topped up` reads as. Every consumer that singles this row out matches on
/// this constant, because the label is the only thing marking it: renaming it at
/// the producer alone silently empties the overview's balance column and drops a
/// DeepSeek account out of the roster's balance rank.
pub(crate) const BALANCE_ROW_LABEL: &str = "api balance";

pub(super) fn matches_base_url(url: &str) -> bool {
    url_matches_host(url, ORIGIN)
}

pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let text = super::get_json(BALANCE_URL, api_key)?;
    let raw: DeepSeekResponse = serde_json::from_str(&text).map_err(|_| ThirdPartyError::Parse)?;
    Ok(stats(&raw))
}

/// Pure response → display-rows mapping, separated from HTTP for testability.
///
/// `is_available` is DeepSeek's own verdict that the balance is sufficient for
/// api calls (its wording, in the reference linked above), never a statement
/// about whether clauth could read one. The response carries `balance_infos`
/// either way, so an unfunded account ships its figures beside the refusal
/// rather than in place of them.
fn stats(raw: &DeepSeekResponse) -> ThirdPartyStats {
    let mut rows: Vec<StatRow> = Vec::new();
    for info in &raw.balance_infos {
        rows.push(StatRow {
            label: format!("{} balance", info.currency),
            value: String::new(),
            kind: StatRowKind::Heading,
        });
        rows.push(StatRow {
            label: BALANCE_ROW_LABEL.to_string(),
            value: format!("{} {}", info.total_balance, info.currency),
            kind: StatRowKind::Body,
        });
        rows.push(StatRow {
            label: "granted".to_string(),
            value: format!("{} {}", info.granted_balance, info.currency),
            kind: StatRowKind::Body,
        });
        rows.push(StatRow {
            label: "topped up".to_string(),
            value: format!("{} {}", info.topped_up_balance, info.currency),
            kind: StatRowKind::Body,
        });
    }
    if raw.is_available {
        ThirdPartyStats::from_rows(rows)
    } else {
        ThirdPartyStats::unfunded(rows)
    }
}

// ── Wire types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct DeepSeekResponse {
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<DeepSeekBalance>,
}

#[derive(Debug, Clone, Deserialize)]
struct DeepSeekBalance {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_deepseek.rs"]
mod tests;
