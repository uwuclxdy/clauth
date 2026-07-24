//! CDX-6: active usage polling for codex accounts — the parked-account gap.
//!
//! The passive JSONL leg (CDX-2) and the proxy header feed (CDX-5) only see
//! accounts that RUN: a parked profile's usage froze at whatever its last
//! live session reported, so a week that reset days ago still rendered as
//! spent (AX report 2026-07-22, ax-codex-xfx). This leg asks the backend
//! directly: `GET https://chatgpt.com/backend-api/wham/usage` with the
//! profile's own stored access token — per-account exact, no live session
//! required.
//!
//! DECISION REVERSAL (2026-07-22, AX): feasibility §2.5 originally banned
//! this endpoint as the ToS-detection path (following loongphy/codex-auth's
//! own README warning). Re-investigated 2026-07-22: codex CLI itself polls
//! the same endpoint ~every 60s (openai/codex#10869), three sibling projects
//! (steipete/CodexBar, mryll/codexbar, MacSteini/Codex-Usage) ship it with
//! no reported incidents, and the risk re-classified as "private API may
//! change without notice", not detection. AX approved read-only polling at
//! codex's own cadence. The docs carry the dated reversal note.
//!
//! INVARIANTS (what keeps this strictly safer than the sibling projects):
//!   * READ-ONLY — this leg never refreshes, never writes auth files, never
//!     touches a refresh token. CDX-3's standby refresh (RotationGuard
//!     single-writer) is the only parked-chain renewer; a 401 here just
//!     waits for it.
//!   * The banned-by-design surfaces stay banned: no `/backend-api/accounts`,
//!     no credit endpoints, no live `~/.codex/auth.json` reads (stored
//!     profile snapshots only).

use serde::Deserialize;

use crate::codex::usage::{LimiterWindow, route_windows};
use crate::usage::UsageInfo;

/// The usage endpoint codex CLI itself polls (~every 60s, openai/codex#10869).
const WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// One poll's outcome, split so the scheduler can pace each differently.
#[derive(Debug)]
pub(crate) enum PollError {
    /// The stored access token was rejected — parked-chain renewal is CDX-3's
    /// job, so the poller stands down until the standby refresh lands.
    Unauthorized,
    /// Endpoint rate limit — widen politely.
    RateLimited,
    /// Transport, unexpected status, or a response shape we don't recognize
    /// (the private API moved). Loud once, then paced.
    Other(String),
}

/// The `wham/usage` response, parsed tolerantly: only the limiter block is
/// read, every unknown field ignored. The LIVE shape (captured 2026-07-22
/// against the real backend): `rate_limit.{primary_window,secondary_window}`
/// with `{used_percent, limit_window_seconds, reset_after_seconds, reset_at}`
/// and a TOP-LEVEL `rate_limit_reached_type`. Aliases also accept the JSONL
/// snapshot spellings (`rate_limits`/`primary`/`resets_at`/`window_minutes`)
/// so either dialect parses; a genuine shape drift degrades to a loud
/// "unrecognized" error, never a zeros publish.
#[derive(Deserialize)]
struct WhamUsage {
    #[serde(alias = "rate_limits")]
    rate_limit: Option<WhamRateLimit>,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
    /// Top-level plan tier (`pro`/`plus`/`free`/…) — the LIVE counterpart of
    /// the stored id_token's `chatgpt_plan_type` claim, which goes stale on a
    /// plan change until codex re-mints.
    #[serde(default)]
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct WhamRateLimit {
    #[serde(alias = "primary")]
    primary_window: Option<WhamWindow>,
    #[serde(alias = "secondary")]
    secondary_window: Option<WhamWindow>,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
}

/// A limiter window in either dialect. Absolute `reset_at`/`resets_at` wins;
/// a relative `reset_after_seconds`/`resets_in_seconds` normalizes to
/// now + delta. Duration arrives as `limit_window_seconds` (live shape) or
/// `window_minutes` (JSONL shape) — [`WhamWindow::into_limiter`] normalizes
/// to minutes for [`route_windows`]'s slotting.
#[derive(Deserialize)]
struct WhamWindow {
    #[serde(default)]
    used_percent: f64,
    #[serde(default, alias = "reset_at")]
    resets_at: Option<i64>,
    #[serde(default, alias = "reset_after_seconds")]
    resets_in_seconds: Option<i64>,
    #[serde(default)]
    window_minutes: Option<i64>,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
}

impl WhamWindow {
    fn into_limiter(self, now_secs: i64) -> LimiterWindow {
        LimiterWindow {
            used_percent: self.used_percent,
            resets_at: self
                .resets_at
                .or_else(|| self.resets_in_seconds.map(|s| now_secs + s)),
            window_minutes: self
                .window_minutes
                .or(self.limit_window_seconds.map(|s| s / 60)),
        }
    }
}

/// What one successful poll yields: the usage snapshot plus the live plan
/// tier riding the same response.
pub(crate) struct PolledUsage {
    pub(crate) info: UsageInfo,
    pub(crate) plan_type: Option<String>,
}

/// Parse a `wham/usage` body into the shared usage shape, through the same
/// duration-based [`route_windows`] slotting the passive leg uses — one
/// mapping, whichever leg carried the data.
pub(crate) fn parse_wham_usage(body: &[u8], now_secs: i64) -> Result<PolledUsage, PollError> {
    let parsed: WhamUsage = serde_json::from_slice(body)
        .map_err(|e| PollError::Other(format!("unrecognized wham/usage shape: {e}")))?;
    let top_verdict = parsed.rate_limit_reached_type;
    let Some(rl) = parsed.rate_limit else {
        return Err(PollError::Other(
            "wham/usage response carries no rate_limit block".to_string(),
        ));
    };
    let (five_hour, seven_day, verdict) = route_windows(
        rl.primary_window.map(|w| w.into_limiter(now_secs)),
        rl.secondary_window.map(|w| w.into_limiter(now_secs)),
        // The live shape stamps the verdict at the TOP level; the JSONL
        // dialect inside the block. Block-level wins when both exist.
        rl.rate_limit_reached_type.or(top_verdict),
    );
    if five_hour.is_none() && seven_day.is_none() {
        return Err(PollError::Other(
            "wham/usage rate_limit block carries no windows".to_string(),
        ));
    }
    Ok(PolledUsage {
        info: UsageInfo {
            five_hour,
            seven_day,
            codex_rate_limit_reached: verdict,
            ..UsageInfo::default()
        },
        plan_type: parsed.plan_type.filter(|p| !p.trim().is_empty()),
    })
}

/// One read-only usage poll with a stored access token. Headers mirror the
/// bare shape mryll/codexbar proved sufficient (Authorization + Accept +
/// account id) — no invented User-Agent to stand out with. The token value
/// itself never reaches a log: errors carry status/shape only.
pub(crate) fn fetch_wham_usage(
    access_token: &str,
    account_id: Option<&str>,
) -> Result<PolledUsage, PollError> {
    let mut req = crate::usage::http_agent()
        .get(WHAM_USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Accept", "application/json");
    if let Some(id) = account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }
    let mut resp = req
        .call()
        .map_err(|e| PollError::Other(format!("wham/usage request failed: {e}")))?;
    match resp.status().as_u16() {
        200 => {}
        401 | 403 => return Err(PollError::Unauthorized),
        429 => return Err(PollError::RateLimited),
        s => return Err(PollError::Other(format!("wham/usage returned HTTP {s}"))),
    }
    let body = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| PollError::Other(format!("wham/usage body read failed: {e}")))?;
    parse_wham_usage(&body, crate::usage::now_ms() as i64 / 1000)
}

#[cfg(test)]
#[path = "../../tests/inline/codex_poll.rs"]
mod tests;
