//! The codex usage leg: `GET .../wham/usage` mapped onto the SAME named window
//! model the claude leg fills, so the chain walk, the TUI columns, and the
//! published feed read a codex account exactly as they read a claude one
//! (settled question 3 — no codex window shape).
//!
//! Everything below is verified against openai/codex `rust-v0.145.0`:
//! `backend-client/src/client/rate_limit_resets.rs` for the endpoint, and the
//! generated wire models under `codex-backend-openapi-models/src/models/` for
//! the body. The body is duration-keyed, not name-keyed — it says "a window of
//! N seconds is P percent spent", never "this is the weekly one" — so the
//! mapping below is where a duration becomes one of clauth's two named slots.

use serde::Deserialize;

use super::fetch::{FetchError, PlanInfo, UsageInfo, UsageWindow, epoch_secs_to_iso, http_agent};

/// The ChatGPT-flavored usage endpoint. codex's client also knows an
/// `/api/codex/usage` spelling for API-key accounts (`PathStyle::CodexApi`);
/// clauth only ever holds ChatGPT logins, so only this one is reachable here.
pub(crate) const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// Anything longer than a day is the weekly slot; anything shorter is the 5h
/// one. Chosen at a day rather than at either nominal length so a server that
/// re-tunes 5h to 4h, or 7d to 30d, still lands on the slot a human means.
const WEEKLY_CUTOFF_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Deserialize)]
struct RawWindow {
    /// 0-100. Integer on the wire; read as f64 because that is what
    /// [`UsageWindow::utilization`] is and every predicate downstream compares.
    #[serde(default)]
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: i64,
    #[serde(default)]
    reset_after_seconds: i64,
    #[serde(default)]
    reset_at: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawRateLimit {
    /// The server's own verdict. `limit_reached` is the HARD one: the account
    /// is blocked now, whatever the percentages say.
    #[serde(default)]
    limit_reached: bool,
    #[serde(default)]
    primary_window: Option<RawWindow>,
    #[serde(default)]
    secondary_window: Option<RawWindow>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawReachedType {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawResetCredits {
    #[serde(default)]
    available_count: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawUsage {
    /// Authoritative over the id_token's `chatgpt_plan_type` claim, which goes
    /// stale the moment a plan changes (settled question 5).
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<RawRateLimit>,
    #[serde(default)]
    rate_limit_reached_type: Option<RawReachedType>,
    /// Banked "reset credits" the account can spend to reopen a window early.
    /// Rides this same response, so reading it costs no extra request.
    #[serde(default)]
    rate_limit_reset_credits: Option<RawResetCredits>,
}

/// Which named slot a window of `secs` belongs to, or `None` when the server
/// sent no usable duration and the caller must fall back to position.
fn slot_for_duration(secs: i64) -> Option<bool> {
    (secs > 0).then_some(secs > WEEKLY_CUTOFF_SECS)
}

/// Turn one raw window into clauth's shape. `reset_at` is the server's absolute
/// answer; `reset_after_seconds` is the relative one, used only when the
/// absolute is missing, so a clock skew between us and the server cannot move a
/// reset the server stated outright.
fn window_from_raw(raw: &RawWindow, now_secs: i64) -> UsageWindow {
    let resets_at = if raw.reset_at > 0 {
        Some(raw.reset_at)
    } else if raw.reset_after_seconds > 0 {
        Some(now_secs + raw.reset_after_seconds)
    } else {
        None
    };
    UsageWindow {
        utilization: raw.used_percent,
        resets_at: resets_at.map(epoch_secs_to_iso),
    }
}

/// Place the two windows into the `(five_hour, seven_day)` slots.
///
/// Duration decides, position breaks ties. The rules, in order:
///  1. a window with a usable `limit_window_seconds` claims the slot its
///     duration names;
///  2. a window without one takes its POSITIONAL slot (primary → 5h,
///     secondary → 7d), which is the layout every observed account has;
///  3. if both windows name the same slot, the first keeps it and the second
///     takes the other slot if it is still free — a collision means the server
///     sent two windows of one kind, and dropping the second would silently
///     lose the tighter of the two limits.
fn place_windows(
    primary: Option<&RawWindow>,
    secondary: Option<&RawWindow>,
    now_secs: i64,
) -> (Option<UsageWindow>, Option<UsageWindow>) {
    let mut five_hour = None;
    let mut seven_day = None;
    for (raw, positional_weekly) in [(primary, false), (secondary, true)] {
        let Some(raw) = raw else { continue };
        let weekly = slot_for_duration(raw.limit_window_seconds).unwrap_or(positional_weekly);
        let mapped = window_from_raw(raw, now_secs);
        let (first, second) = if weekly {
            (&mut seven_day, &mut five_hour)
        } else {
            (&mut five_hour, &mut seven_day)
        };
        if first.is_none() {
            *first = Some(mapped);
        } else if second.is_none() {
            *second = Some(mapped);
        }
    }
    (five_hour, seven_day)
}

/// The whole body → [`UsageInfo`] mapping, split from the HTTP leg so every
/// rule above is testable against a literal body.
pub(crate) fn map_usage(body: &str, now_secs: i64) -> Result<UsageInfo, FetchError> {
    let raw: RawUsage = serde_json::from_str(body).map_err(|_| FetchError::Parse)?;
    let rate_limit = raw.rate_limit.unwrap_or_default();
    let (mut five_hour, mut seven_day) = place_windows(
        rate_limit.primary_window.as_ref(),
        rate_limit.secondary_window.as_ref(),
        now_secs,
    );

    // The server's hard verdict outranks its own percentages: `limit_reached`
    // means blocked NOW, and an account blocked at 96% would otherwise sit
    // below every threshold and keep being chosen. The block is attributed to
    // the fuller window — which is what "renamed to the slot its window landed
    // in" means once the windows have slots — and never fabricated onto a
    // window the body did not send.
    if rate_limit.limit_reached {
        let five = five_hour.as_ref().map_or(-1.0, |w| w.utilization);
        let seven = seven_day.as_ref().map_or(-1.0, |w| w.utilization);
        let blocking = if seven > five {
            seven_day.as_mut()
        } else {
            five_hour.as_mut()
        };
        if let Some(w) = blocking {
            w.utilization = w.utilization.max(100.0);
        }
    }

    Ok(UsageInfo {
        plan: Some(PlanInfo {
            codex_plan: raw.plan_type.map(|p| p.trim().to_ascii_lowercase()),
            ..PlanInfo::default()
        }),
        five_hour,
        seven_day,
        codex_limit_reached: raw
            .rate_limit_reached_type
            .and_then(|r| r.kind)
            .filter(|k| !k.is_empty()),
        codex_reset_credits: raw.rate_limit_reset_credits.map(|c| c.available_count),
        ..UsageInfo::default()
    })
}

/// Poll one codex account's usage. Read-only: this endpoint is the only codex
/// HTTP surface clauth touches outside a refresh, and it neither mints nor
/// spends anything.
///
/// A 401 is the caller's signal that the access token is stale — it feeds
/// [`crate::codex_auth::kick_codex`], the only producer of that queue.
pub(crate) fn fetch_codex_usage(
    access_token: &str,
    account_id: Option<&str>,
    now_secs: i64,
) -> Result<UsageInfo, FetchError> {
    let mut req = http_agent()
        .get(CODEX_USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Accept", "application/json");
    // Multi-workspace logins answer for whichever account this header names;
    // without it the server picks, which is not necessarily the profile's.
    if let Some(id) = account_id.map(str::trim).filter(|id| !id.is_empty()) {
        req = req.header("chatgpt-account-id", id);
    }
    let mut response = req.call().map_err(|_| FetchError::Network)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(FetchError::Status(status));
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|_| FetchError::Network)?;
    map_usage(&body, now_secs)
}

#[cfg(test)]
#[path = "../../tests/inline/codex_usage.rs"]
mod tests;
