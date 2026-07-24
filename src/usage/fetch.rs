use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::lockorder::{RankedMutex, rank};
use crate::profile_cache::{
    ACCOUNT_EMAIL_CACHE_FILE, ACCOUNT_ID_CACHE_FILE, PROFILE_FETCHED_CACHE_FILE,
    load_profile_cache, remove_profile_cache, write_profile_cache,
};

use super::scheduler::{ActivityStore, ProfileActivity, mark_activity};

const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/profile";

/// Re-fetch `/profile` (plan / rate-limit tier) at most once per hour per
/// profile. The tier rarely changes, so the steady usage poll reuses the cached
/// plan and only hits `/profile` on first load, after a 401 rotation, once the
/// hour lapses, or on a manual single-profile refresh (which expires this
/// clock). Halves the steady request volume against the rate-limited host.
const PROFILE_TTL_MS: u64 = 60 * 60 * 1000;

/// Per-profile epoch-ms of the last `/profile` fetch attempt — the in-memory half
/// of the TTL clock for the policy above, backed by a durable per-profile stamp
/// ([`PROFILE_FETCHED_CACHE_FILE`]) so the hour survives a restart. A true leaf
/// (`rank::ProfileTtl`): every acquisition is take-read/insert-release and none
/// spans the stamp's disk IO, which is what lets the rank sit late enough for its
/// real holders (`Rotation` on the post-401 retry, `Config` on an account swap).
static PROFILE_FETCHED: LazyLock<RankedMutex<HashMap<String, u64>, rank::ProfileTtl>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// Minimum spacing between consecutive requests to the same endpoint host,
/// enforced process-wide and keyed per host (see [`NEXT_REQUEST_SLOT`]). Accounts
/// sharing a host (every Anthropic OAuth account hits `api.anthropic.com`) pace this
/// far apart so a same-instant multi-profile burst (startup, refetch-queue drains, a
/// window-reset kick fan-out) can't trip a 429; accounts on distinct hosts (each
/// api-key provider) reserve independent slots and never wait on each other. Steady
/// polling sits well below this rate, so it only bites on bursts.
const REQUEST_SPACING_MS: u64 = 5_000;

/// Origin all OAuth `/usage`, `/profile`, and `/v1/messages` kick requests target —
/// they are hardcoded to this host regardless of a profile's `base_url`, so it is
/// their per-host pacing key in [`NEXT_REQUEST_SLOT`].
pub(crate) const ANTHROPIC_ORIGIN: &str = "https://api.anthropic.com";

/// Earliest epoch-ms the next request to each host may fire, keyed by endpoint
/// origin. Each caller reserves its host's next free slot (advancing it by
/// [`REQUEST_SPACING_MS`]) and sleeps until then. Leaf-ranked and held only to
/// reserve the slot — never across the sleep or the HTTP round trip.
static NEXT_REQUEST_SLOT: LazyLock<RankedMutex<HashMap<String, u64>, rank::UsageThrottle>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// Pure slot reservation: from a host's current earliest-allowed slot and `now`,
/// return `(advanced_slot, wait_ms)` — the slot reserved for the next caller on that
/// host (one [`REQUEST_SPACING_MS`] past this caller's fire time) and how long this
/// caller must wait for its own slot.
fn reserve_slot(current_slot: u64, now: u64) -> (u64, u64) {
    let fire_at = current_slot.max(now);
    (
        fire_at.saturating_add(REQUEST_SPACING_MS),
        fire_at.saturating_sub(now),
    )
}

/// Block until this caller's spacing slot for `host`, reserving the following slot
/// for the next caller on the same host. Distinct hosts hold independent slots, so
/// requests to different endpoints never serialize against each other. A poisoned
/// lock skips throttling rather than stalling the fetch.
pub(crate) fn await_request_slot(host: &str) {
    let now = now_ms();
    let wait_ms = {
        let Ok(mut slots) = NEXT_REQUEST_SLOT.lock() else {
            return;
        };
        let slot = slots.entry(host.to_string()).or_insert(0);
        let (next, wait) = reserve_slot(*slot, now);
        *slot = next;
        wait
    };
    if wait_ms > 0 {
        std::thread::sleep(Duration::from_millis(wait_ms));
    }
}

/// Clear every host's reserved spacing slot. Test-only: a real-bytes listener
/// test that drives a request builder through [`await_request_slot`] resets the
/// slot first so it never sleeps out the [`REQUEST_SPACING_MS`] window under the
/// shared-process `cargo test` runner (nextest isolates per process).
#[cfg(test)]
pub(crate) fn reset_request_slots() {
    if let Ok(mut slots) = NEXT_REQUEST_SLOT.lock() {
        slots.clear();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageWindow {
    pub(crate) utilization: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resets_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ExtraUsage {
    #[serde(default)]
    pub(crate) is_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) monthly_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used_credits: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) utilization: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) currency: Option<String>,
    /// Per-period credit breakdowns (`daily`/`weekly`) — shape is not yet
    /// observable on any account, so they're held as raw JSON and read
    /// defensively at render time (see [`ExtraPeriod::from_value`]); a number,
    /// object, or null all parse without breaking the `/usage` body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daily: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) weekly: Option<serde_json::Value>,
}

/// A defensively-extracted view of an `extra_usage.daily`/`weekly` sub-object.
/// The real shape is undocumented and null on every reachable account, so this
/// pulls only the numeric fields it recognizes and treats anything else as
/// absent — never a parse failure.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExtraPeriod {
    pub(crate) used_credits: Option<f64>,
    pub(crate) utilization: Option<f64>,
    pub(crate) monthly_limit: Option<f64>,
    pub(crate) currency: Option<String>,
}

impl ExtraPeriod {
    /// Pull the recognized numeric fields out of a raw `daily`/`weekly` value.
    /// `None` when the value carries nothing renderable.
    pub(crate) fn from_value(v: &serde_json::Value) -> Option<Self> {
        let obj = v.as_object()?;
        let num = |k: &str| obj.get(k).and_then(serde_json::Value::as_f64);
        let p = ExtraPeriod {
            used_credits: num("used_credits"),
            utilization: num("utilization"),
            monthly_limit: num("monthly_limit"),
            currency: obj
                .get("currency")
                .and_then(|c| c.as_str())
                .map(str::to_string),
        };
        (p.utilization.is_some() || p.used_credits.is_some()).then_some(p)
    }
}

/// Absolute used/limit dollar figures for a usage window, from the raw
/// `used_dollars`/`limit_dollars` fields. The wire shape is undetermined (Claude
/// Code's own client ignores these), so they're parsed leniently (see
/// [`json_to_dollars`]) and only surface when a value is actually present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowDollars {
    pub(crate) label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<f64>,
}

/// Lenient money → dollars. Accepts a bare number (already dollars, per the
/// `_dollars` field name), a `{amount_minor, exponent}` minor-unit object, or an
/// `{amount}` object (number or numeric string). Anything else → `None`, never a
/// panic or parse error — the wire shape is unconfirmed.
fn json_to_dollars(v: &serde_json::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let obj = v.as_object()?;
    if let Some(minor) = obj.get("amount_minor").and_then(serde_json::Value::as_i64) {
        let exp = obj
            .get("exponent")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(2) as i32;
        return Some(minor as f64 / 10f64.powi(exp));
    }
    match obj.get("amount") {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// A per-model weekly window derived from a `weekly_scoped` entry in the
/// `/usage` `limits[]` array. `label` is built from the scope's model name
/// (`"7d fable"`, `"7d opus"`, …), so a model the server adds later shows up as
/// a bar with no code change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScopedWindow {
    pub(crate) label: String,
    #[serde(flatten)]
    pub(crate) window: UsageWindow,
}

/// Pay-as-you-go spend / credit cap from the `/usage` `spend` block. Distinct
/// from [`ExtraUsage`] (the legacy credits field): the API now returns both, so
/// each renders its own bar when populated. Dollar figures are normalized from
/// the API's minor-unit money objects.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SpendInfo {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) currency: Option<String>,
}

impl SpendInfo {
    /// Build from the raw `spend` block, converting minor-unit money to dollars.
    fn from_raw(s: &RawSpend) -> Self {
        SpendInfo {
            enabled: s.enabled,
            used: s.used.as_ref().and_then(RawMoney::to_dollars),
            limit: s.limit.as_ref().and_then(RawMoney::to_dollars),
            percent: s.percent,
            currency: s
                .used
                .as_ref()
                .or(s.limit.as_ref())
                .and_then(|m| m.currency.clone()),
        }
    }

    /// A spend bar is worth showing once the account has a cap enabled or a
    /// limit set; disabled accounts (the current default) render nothing.
    pub(crate) fn is_visible(&self) -> bool {
        self.enabled || self.limit.is_some()
    }
}

/// Canonical account tier, computed once at fetch time. The single source of
/// truth that `plan_label` / `endpoint_label` render from — collapses the old
/// four-field `PlanInfo` fan-out into one enum. `Serialize`/`Deserialize` keep it
/// in the `usage_cache.json` shape; a field rename simply misses → refetches.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) enum PlanTier {
    Max(#[serde(default)] Option<u16>),
    Pro,
    Team,
    Enterprise,
    Free,
    #[default]
    Unknown,
}

impl PlanTier {
    /// Reproduce the old `plan_label` classification exactly from the raw
    /// `/profile` fields. `rate_limit_tier` carries the Max multiplier when present.
    pub(crate) fn from_profile(
        org_type: Option<&str>,
        has_max: bool,
        has_pro: bool,
        rate_limit_tier: Option<&str>,
    ) -> Self {
        match org_type.unwrap_or("") {
            "claude_max" => PlanTier::Max(max_multiplier(rate_limit_tier)),
            "claude_pro" => PlanTier::Pro,
            "claude_team" | "claude_teams" => PlanTier::Team,
            "claude_enterprise" => PlanTier::Enterprise,
            "claude_free" | "free" => PlanTier::Free,
            "" => {
                if has_max {
                    PlanTier::Max(None)
                } else if has_pro {
                    PlanTier::Pro
                } else {
                    PlanTier::Unknown
                }
            }
            _ => PlanTier::Unknown,
        }
    }

    /// Map the OAuth token's `subscription_type` so a not-yet-fetched profile
    /// still shows a sane tier label. A missing claim (a credential-less or
    /// never-fetched profile) or an unrecognized value → `Unknown`, never a
    /// fabricated paid tier: `Unknown` renders neutrally (`short_label` omits it,
    /// `display` shows the bare "Claude") instead of a "Pro" out of thin air.
    pub(crate) fn from_subscription_type(s: Option<&str>) -> Self {
        match s {
            Some("pro") => PlanTier::Pro,
            Some("max") => PlanTier::Max(None),
            Some("team") | Some("teams") => PlanTier::Team,
            Some("enterprise") => PlanTier::Enterprise,
            _ => PlanTier::Unknown,
        }
    }

    /// Same strings the old `plan_label` emitted, for every tier.
    pub(crate) fn display(&self) -> String {
        match self {
            PlanTier::Max(Some(n)) => format!("Claude Max {n}x"),
            PlanTier::Max(None) => "Claude Max".to_string(),
            PlanTier::Pro => "Claude Pro".to_string(),
            PlanTier::Team => "Claude Team".to_string(),
            PlanTier::Enterprise => "Claude Enterprise".to_string(),
            PlanTier::Free => "Claude Free".to_string(),
            PlanTier::Unknown => "Claude".to_string(),
        }
    }

    /// Compact tier label without the `Claude ` prefix, for contexts that
    /// already name the provider (e.g. the MCP inventory's `[anthropic, …]`).
    /// `None` for an unknown tier so callers can omit it entirely.
    pub(crate) fn short_label(&self) -> Option<String> {
        Some(match self {
            PlanTier::Max(Some(n)) => format!("Max {n}x"),
            PlanTier::Max(None) => "Max".to_string(),
            PlanTier::Pro => "Pro".to_string(),
            PlanTier::Team => "Team".to_string(),
            PlanTier::Enterprise => "Enterprise".to_string(),
            PlanTier::Free => "Free".to_string(),
            PlanTier::Unknown => return None,
        })
    }
}

/// Pull the trailing `Nx` multiplier out of a rate-limit tier like
/// `default_claude_max_5x` / `default_claude_max_20x`.
fn max_multiplier(tier: Option<&str>) -> Option<u16> {
    let tier = tier?;
    let last = tier.rsplit('_').next()?;
    last.strip_suffix('x').and_then(|m| {
        m.chars()
            .all(|c| c.is_ascii_digit())
            .then(|| m.parse().ok())?
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PlanInfo {
    #[serde(default)]
    pub(crate) tier: PlanTier,
    /// `/profile` `organization.subscription_status` verbatim (`active`,
    /// `trialing`, `canceled`, …). A canceled subscription drops the org to the
    /// `claude_free` tier while its 5h window stays cached, so the raw status is
    /// the only proof the account is dead rather than a genuine free plan. Kept
    /// as the raw string (not a closed enum) so an unrecognized future status
    /// never fails the whole `usage_cache.json` parse — same fail-to-refetch
    /// contract the `PlanTier` serde note carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subscription_status: Option<String>,
}

impl PlanInfo {
    /// A precisely canceled subscription — distinct from a never-subscribed Free
    /// account, whose status is absent or something other than `canceled`.
    pub(crate) fn is_canceled(&self) -> bool {
        self.subscription_status.as_deref() == Some("canceled")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct UsageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<PlanInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seven_day: Option<UsageWindow>,
    /// Per-model weekly windows (`weekly_scoped` limits) in `limits[]` order —
    /// grows as the server exposes new models, no per-model field needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) weekly_scoped: Vec<ScopedWindow>,
    /// Absolute dollar figures per window label (`5h`/`7d`), when the endpoint
    /// carries `used_dollars`/`limit_dollars`. Empty on every current account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) window_dollars: Vec<WindowDollars>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extra_usage: Option<ExtraUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<SpendInfo>,
    /// CDX-4 §0.16: codex's own limiter verdict (`rate_limit_reached_type`
    /// from the session-JSONL snapshot — e.g. `primary`/`secondary`). Codex
    /// profiles only; claude fetch paths never set it. Carried HERE so the
    /// one struct that already flows cache → store → status.json feeds the
    /// codex chain scan, the status serializer, and ccu with a single source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) codex_rate_limit_reached: Option<String>,
}

/// Fixed labels for the two always-present windows. Per-model weekly labels are
/// built dynamically from the scope name (see [`ScopedWindow`]).
pub(crate) const LABEL_5H: &str = "5h";
pub(crate) const LABEL_7D: &str = "7d";

impl UsageInfo {
    /// All available windows as `(label, &UsageWindow)` pairs: 5h, 7d, then each
    /// per-model weekly window in `limits[]` order.
    pub(crate) fn windows(&self) -> Vec<(&str, &UsageWindow)> {
        let mut out = Vec::new();
        if let Some(w) = &self.five_hour {
            out.push((LABEL_5H, w));
        }
        if let Some(w) = &self.seven_day {
            out.push((LABEL_7D, w));
        }
        for s in &self.weekly_scoped {
            out.push((s.label.as_str(), &s.window));
        }
        out
    }

    /// Most representative weekly window: the aggregate `seven_day` when present,
    /// else the first per-model window.
    pub(crate) fn weekly_window(&self) -> Option<&UsageWindow> {
        self.seven_day
            .as_ref()
            .or_else(|| self.weekly_scoped.first().map(|s| &s.window))
    }
}

/// Nominal length of the rolling window named by `label`, in seconds. `None`
/// for labels with no fixed window (e.g. the monthly extra-credits bar).
pub(crate) fn window_duration_secs(label: &str) -> Option<i64> {
    if label == LABEL_5H {
        Some(5 * 3600)
    } else if label == LABEL_7D || label.starts_with("7d ") {
        // `7d` plus every per-model weekly label (`"7d fable"`, `"7d opus"`, …).
        Some(7 * 86_400)
    } else {
        // Provider window labels of the form `<n>h` / `<n>d` (e.g. z.ai's
        // `5h`/`7d`/`30d`) so any api-key account with a windowed limit gets the
        // same average pace + ideal-pace line as the OAuth windows.
        parse_nh_nd_label(label)
    }
}

/// Parse a `"<n>h"` / `"<n>d"` window label into a duration in seconds. `None`
/// for any other shape.
fn parse_nh_nd_label(label: &str) -> Option<i64> {
    let (num, unit) = label.split_at(label.len().checked_sub(1)?);
    let n = num.parse::<i64>().ok().filter(|&n| n > 0)?;
    match unit {
        "h" => Some(n * 3600),
        "d" => Some(n * 86_400),
        _ => None,
    }
}

/// Ideal-pace percentage (0..=100) for a usage window at `now_secs`: the share
/// of the window already elapsed. Usage spread evenly across the window tracks
/// this line, so a fill past it is ahead of pace and a fill behind it is under
/// pace. `None` when the window has no reset time or no fixed duration.
pub(crate) fn ideal_pace_pct(label: &str, window: &UsageWindow, now_secs: i64) -> Option<f64> {
    let duration = window_duration_secs(label)?;
    let reset = iso_to_epoch_secs(window.resets_at.as_deref()?)?;
    let remaining = (reset - now_secs).clamp(0, duration);
    let elapsed = duration - remaining;
    Some(elapsed as f64 / duration as f64 * 100.0)
}

/// Average burn pace in %/day for `window`: utilization spread evenly over the
/// time elapsed since the window opened (`resets_at − duration`). Unlike the
/// recency-weighted recent-burn rate, this is anchored to the fixed window, so
/// it is unaffected by account rotation (which makes a per-profile history jump
/// to another account's utilization). `None` until `min_elapsed_secs` have
/// elapsed — a freshly opened window would otherwise divide by ~0 — or when the
/// window has no reset time or no fixed duration.
pub(crate) fn window_avg_pace_per_day(
    label: &str,
    window: &UsageWindow,
    now_secs: i64,
    min_elapsed_secs: i64,
) -> Option<f64> {
    let duration = window_duration_secs(label)?;
    let reset = iso_to_epoch_secs(window.resets_at.as_deref()?)?;
    let remaining = (reset - now_secs).clamp(0, duration);
    let elapsed = duration - remaining;
    if elapsed < min_elapsed_secs {
        return None;
    }
    Some(window.utilization / (elapsed as f64 / 86_400.0))
}

#[derive(Deserialize)]
struct RawUsage {
    // Legacy top-level windows — the fallback for when `limits[]` omits a
    // `session` / `weekly_all` entry, and the only carrier of the per-window
    // `*_dollars` figures (they never appear on `limits[]` entries).
    #[serde(default)]
    five_hour: Option<RawWindow>,
    #[serde(default)]
    seven_day: Option<RawWindow>,
    /// Normalized rate-limit list — the source of truth for every window.
    #[serde(default)]
    limits: Vec<RawLimit>,
    #[serde(default)]
    extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    spend: Option<RawSpend>,
}

/// A top-level window object (`five_hour`/`seven_day`). Carries the percentage +
/// reset like [`UsageWindow`] plus the lenient `*_dollars` figures held as raw
/// JSON (undetermined shape).
#[derive(Deserialize)]
struct RawWindow {
    #[serde(default)]
    utilization: f64,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    used_dollars: Option<serde_json::Value>,
    #[serde(default)]
    limit_dollars: Option<serde_json::Value>,
}

impl RawWindow {
    fn to_window(&self) -> UsageWindow {
        UsageWindow {
            utilization: self.utilization,
            resets_at: self.resets_at.clone(),
        }
    }

    /// The window's absolute dollar figures, labeled, or `None` when neither
    /// `used_dollars` nor `limit_dollars` resolves to a number.
    fn dollars(&self, label: &str) -> Option<WindowDollars> {
        let used = self.used_dollars.as_ref().and_then(json_to_dollars);
        let limit = self.limit_dollars.as_ref().and_then(json_to_dollars);
        (used.is_some() || limit.is_some()).then(|| WindowDollars {
            label: label.to_string(),
            used,
            limit,
        })
    }
}

/// One entry of the `/usage` `limits[]` array. `kind` selects the window
/// (`session` → 5h, `weekly_all` → 7d, `weekly_scoped` → per-model); `scope`
/// carries the model name for scoped entries. `is_active` is intentionally not
/// read — the array is already scoped to what applies, and 5h/7d must show
/// regardless of it.
#[derive(Deserialize)]
struct RawLimit {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Option<RawScope>,
}

#[derive(Deserialize)]
struct RawScope {
    #[serde(default)]
    model: Option<RawScopeModel>,
    /// Consumption surface (web / desktop / code / …) for a surface-scoped
    /// limit. Undetermined shape (string vs `{id, display_name}`), so held as
    /// raw JSON and read leniently by [`RawScope::label`].
    #[serde(default)]
    surface: Option<serde_json::Value>,
}

impl RawScope {
    /// Human label for a scoped limit: the model's display name when present,
    /// else the surface name (string, or an object's `display_name`/`id`).
    fn label(&self) -> Option<String> {
        if let Some(name) = self.model.as_ref().and_then(|m| m.display_name.as_deref()) {
            return Some(name.to_string());
        }
        let surface = self.surface.as_ref()?;
        if let Some(s) = surface.as_str() {
            return Some(s.to_string());
        }
        let obj = surface.as_object()?;
        obj.get("display_name")
            .or_else(|| obj.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

#[derive(Deserialize)]
struct RawScopeModel {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawSpend {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    used: Option<RawMoney>,
    #[serde(default)]
    limit: Option<RawMoney>,
    #[serde(default)]
    percent: Option<f64>,
}

/// A minor-unit money object (`{amount_minor, currency, exponent}`) as returned
/// under `spend`. `exponent` defaults to 2 (cents) when absent.
#[derive(Deserialize)]
struct RawMoney {
    #[serde(default)]
    amount_minor: Option<i64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    exponent: Option<i32>,
}

impl RawMoney {
    fn to_dollars(&self) -> Option<f64> {
        Some(self.amount_minor? as f64 / 10f64.powi(self.exponent.unwrap_or(2)))
    }
}

/// The window set derived from a parsed `/usage` body.
#[derive(Default)]
struct DerivedWindows {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    weekly_scoped: Vec<ScopedWindow>,
    window_dollars: Vec<WindowDollars>,
}

/// Derive the window set from a parsed `/usage` body. `limits[]` is the source
/// of truth: `session` → 5h, `weekly_all` → 7d, and each `weekly_scoped` entry
/// becomes a dynamic `"7d <model>"` window (from the scope's model or surface
/// name) — so a model the server adds later is picked up automatically. A
/// missing `session` / `weekly_all` limit falls back to the legacy top-level
/// field, which is also the only carrier of the per-window `*_dollars` figures.
fn windows_from_raw(raw: &RawUsage) -> DerivedWindows {
    let mut five = None;
    let mut seven = None;
    let mut scoped = Vec::new();
    for limit in &raw.limits {
        let window = UsageWindow {
            utilization: limit.percent.unwrap_or(0.0),
            resets_at: limit.resets_at.clone(),
        };
        match limit.kind.as_deref() {
            Some("session") => five = Some(window),
            Some("weekly_all") => seven = Some(window),
            Some("weekly_scoped") => {
                if let Some(name) = limit.scope.as_ref().and_then(RawScope::label) {
                    scoped.push(ScopedWindow {
                        label: format!("{LABEL_7D} {}", name.to_lowercase()),
                        window,
                    });
                }
            }
            _ => {}
        }
    }
    // Dollar figures ride only the top-level window objects, never `limits[]`.
    let window_dollars = [(LABEL_5H, &raw.five_hour), (LABEL_7D, &raw.seven_day)]
        .into_iter()
        .filter_map(|(label, w)| w.as_ref().and_then(|w| w.dollars(label)))
        .collect();
    DerivedWindows {
        five_hour: five.or_else(|| raw.five_hour.as_ref().map(RawWindow::to_window)),
        seven_day: seven.or_else(|| raw.seven_day.as_ref().map(RawWindow::to_window)),
        weekly_scoped: scoped,
        window_dollars,
    }
}

#[derive(Deserialize)]
struct RawProfile {
    #[serde(default)]
    account: Option<RawProfileAccount>,
    #[serde(default)]
    organization: Option<RawProfileOrg>,
}

#[derive(Deserialize)]
struct RawProfileAccount {
    #[serde(default)]
    uuid: Option<String>,
    /// The account's email — operator-readable identity half (the live
    /// response uses `email`; the alias tolerates snake_case drift).
    #[serde(default, alias = "email_address")]
    email: Option<String>,
    #[serde(default)]
    has_claude_max: bool,
    #[serde(default)]
    has_claude_pro: bool,
}

#[derive(Deserialize)]
struct RawProfileOrg {
    #[serde(default)]
    organization_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
    #[serde(default)]
    subscription_status: Option<String>,
}

/// HTTP layer error. `Status` carries an HTTP code so the fetch path can
/// distinguish a 401 (refresh + retry) from a connection blip (cache); a 429
/// gets its own variant carrying the server's `retry-after` hint (rate-limited,
/// cache — never rotate, defer the next attempt).
pub(super) enum FetchError {
    Status(u16),
    /// HTTP 429. `retry_after` is the server's `retry-after` header when
    /// present (delta-seconds or an IMF HTTP-date); an unparseable value is
    /// absent, and a `0` / past date parses to `ZERO` ("retry now"). `plan`
    /// carries a `/profile` reading taken DESPITE the 429: a canceled account
    /// 429s `/usage` forever, so the profile leg is the only place its
    /// cancellation is ever observed. `None` from the low-level `get_json`
    /// (no profile context there); populated by [`fetch_raw`].
    RateLimited {
        retry_after: Option<Duration>,
        plan: Option<PlanInfo>,
    },
    Network,
    Parse,
}

/// Parse a `retry-after` header value into a delay from now. Accepts the
/// delta-seconds form (`120`) and the IMF-fixdate HTTP-date form
/// (`Wed, 21 Oct 2015 07:28:00 GMT`); a past date yields `Duration::ZERO` and
/// anything else returns `None` — no usable hint.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    parse_retry_after_at(value, now_epoch_secs())
}

/// Pure core of [`parse_retry_after`] taking the reference instant, so the
/// HTTP-date branch is deterministic under test.
pub(crate) fn parse_retry_after_at(value: &str, now_secs: i64) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target = httpdate_to_epoch_secs(value)?;
    Some(Duration::from_secs(
        target.saturating_sub(now_secs).max(0) as u64
    ))
}

/// Parse an HTTP-date in IMF-fixdate form (`Wed, 21 Oct 2015 07:28:00 GMT`) to
/// Unix epoch seconds. The obsolete RFC-850 / asctime forms and anything
/// malformed return `None`.
fn httpdate_to_epoch_secs(value: &str) -> Option<i64> {
    let mut parts = value.split_ascii_whitespace();
    parts.next()?; // day-of-week (e.g. "Wed,") — unused
    let day: i64 = parts.next()?.parse().ok()?;
    let month: i64 = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.parse().ok()?;
    if hms.next().is_some() || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        // ureq 3 defaults non-2xx to `Err(Error::StatusCode)`; our callers read
        // the status off the `Ok` response (401 → rotate, 429 → retry-after).
        // Without this flag those branches are unreachable and every HTTP error
        // collapses into `Network`.
        .http_status_as_error(false)
        .build()
        .into()
});

/// Shared HTTP agent for usage-style GETs (also used by `crate::providers`).
/// Status codes arrive on the `Ok` response — see the builder comment.
pub(crate) fn http_agent() -> &'static ureq::Agent {
    &AGENT
}

/// `User-Agent` for `/usage` + `/profile` requests. Anthropic rate-limits this
/// endpoint far harder for clients that don't identify as Claude Code
/// (anthropics/claude-code#31637), so mimic its UA byte-for-byte: CC's axios
/// client sends `claude-cli/<version> (external, cli)` (verified on the wire —
/// `cli` is the interactive entrypoint tag). Version resolved once per process
/// from the locally-detected CC; falls back to a bare `claude-cli`.
static USER_AGENT: LazyLock<String> =
    LazyLock::new(|| match crate::plugin_probe::cc_version().as_deref() {
        Some(v) => match v.split_whitespace().next() {
            Some(ver) if !ver.is_empty() => format!("claude-cli/{ver} (external, cli)"),
            _ => "claude-cli".to_string(),
        },
        None => "claude-cli".to_string(),
    });

/// CC's `claude-cli/<ver> (external, cli)` User-Agent, shared by every request
/// that identifies as the interactive CLI client: `/usage` and the `/v1/messages`
/// window kick (`crate::oauth::kick`). One source of truth so the kick can't
/// drift back to ureq's default UA — the header the rate limiter keys on hardest.
pub(crate) fn cli_user_agent() -> &'static str {
    USER_AGENT.as_str()
}

/// Which of Claude Code's two `api.anthropic.com` clients to imitate. CC polls
/// `/usage` with its `claude-cli` client but reads `/profile` through a plain
/// axios instance — different UA, and `/profile` carries `Cache-Control: no-cache`
/// with no `anthropic-beta`. See `docs/wire-parity.md`.
#[derive(Clone, Copy)]
enum AuthClient {
    /// `/usage`: `claude-cli/<ver> (external, cli)` UA + `anthropic-beta`.
    Usage,
    /// `/profile`: `axios/1.15.2` UA + `Cache-Control: no-cache`, no beta.
    Profile,
}

fn get_json(
    url: &str,
    access_token: &str,
    activity: Option<&ActivityStore>,
    name: &str,
    client: AuthClient,
) -> std::result::Result<String, FetchError> {
    await_request_slot(ANTHROPIC_ORIGIN);
    // The throttle wait is over and the request is about to leave the gate — flip
    // the spinner from `Queued` to `Fetching` so only the profile actually in
    // flight reads as fetching, not the whole batch waiting behind the spacing.
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Fetching);
    }
    // Both CC clients send Accept + Content-Type (the latter even without a body);
    // the UA and the beta/cache-control headers are what split the two.
    let req = AGENT
        .get(url)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json");
    let req = match client {
        AuthClient::Usage => req
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", USER_AGENT.as_str()),
        AuthClient::Profile => req
            .header("User-Agent", crate::oauth::TOKEN_USER_AGENT)
            .header("Cache-Control", "no-cache"),
    };
    let mut response = req.call().map_err(|_| FetchError::Network)?;
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        return Err(FetchError::RateLimited {
            retry_after,
            plan: None,
        });
    }
    if status >= 400 {
        return Err(FetchError::Status(status));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|_| FetchError::Network)
}

/// Mark `name`'s plan stale so the next fetch re-pulls `/profile` — the manual
/// single-profile refresh (Usage `r` / action menu). A global "refresh all"
/// deliberately does not call this, so it keeps reusing the cached plan. Clears
/// BOTH halves of the clock: dropping only the map entry would leave the durable
/// stamp to be read straight back in as fresh, silently reducing the manual
/// refresh to a no-op for `/profile`.
pub(crate) fn expire_profile_ttl(name: &str) {
    if let Ok(mut m) = PROFILE_FETCHED.lock() {
        m.remove(name);
    }
    remove_profile_cache(name, PROFILE_FETCHED_CACHE_FILE);
}

/// Drop `name`'s in-memory memo while leaving the durable stamp in place — what
/// a process restart looks like to [`take_profile_fetch`], which is the whole
/// point of the durable half and can't otherwise be exercised in one process.
#[cfg(test)]
fn forget_profile_memo(name: &str) {
    if let Ok(mut m) = PROFILE_FETCHED.lock() {
        m.remove(name);
    }
}

/// The profile has a usable identity anchor. A blank/whitespace uuid is shape
/// drift, never an identity — same contract as [`seed_identity_anchor`] and
/// [`fetch_account_uuid`].
fn has_identity_anchor(name: &str) -> bool {
    load_profile_cache::<String>(name, ACCOUNT_ID_CACHE_FILE).is_some_and(|u| !u.trim().is_empty())
}

/// The last `/profile` attempt stamp for `name`, honouring the durable stamp only
/// once the profile is anchored. `seed_identity_anchor` backfills the anchor as a
/// ride-along on the `/profile` body, so trusting the stamp of an anchor-less
/// profile would defer that backfill by up to an hour — and it is exactly the
/// unanchored profile that needs it, since without an anchor a dead stored pair
/// wedges the profile in `auth_broken`. Unanchored profiles therefore pay one
/// `/profile` per launch until the first backfill lands, then join everyone else.
fn last_profile_attempt(name: &str) -> Option<u64> {
    if let Some(t) = PROFILE_FETCHED
        .lock()
        .ok()
        .and_then(|m| m.get(name).copied())
    {
        return Some(t);
    }
    if !has_identity_anchor(name) {
        return None;
    }
    // Cold map (process start): adopt the durable stamp and memoize it, so the
    // disk is read at most once per profile per process. The lock is taken fresh
    // here — never held across the read above.
    let disk = load_profile_cache::<u64>(name, PROFILE_FETCHED_CACHE_FILE)?;
    if let Ok(mut m) = PROFILE_FETCHED.lock() {
        m.insert(name.to_string(), disk);
    }
    Some(disk)
}

/// Decide whether to fetch `/profile` this round and stamp the attempt. Fetches
/// when `force` is set (a rotation retry holding no plan yet), on first load (no
/// stamp on disk either), or once the hourly TTL lapses. A manual single-profile
/// refresh arrives here as a lapsed clock, not a force: it calls
/// [`expire_profile_ttl`] first. Stamping on attempt — success or
/// failure alike — caps `/profile` at one hit per hour per profile, so a
/// persistently failing endpoint can't turn into a per-tick storm (the plan is
/// best-effort; a cold profile just shows no tier until the next hourly try).
///
/// A stamp in the FUTURE (clock rollback: an NTP correction, a VM restore) is not
/// freshness and never counts as such — `checked_sub` fails toward fetching, which
/// also re-stamps the bogus clock back to sanity. Saturating the age to `0` here
/// would instead mute `/profile` until wall-clock caught up, and — now that the
/// stamp outlives the process — it would stay muted across every restart.
pub(crate) fn take_profile_fetch(name: &str, force: bool, now: u64) -> bool {
    let fresh = last_profile_attempt(name)
        .and_then(|t| now.checked_sub(t))
        .is_some_and(|age| age < PROFILE_TTL_MS);
    let want = force || !fresh;
    if want {
        if let Ok(mut m) = PROFILE_FETCHED.lock() {
            m.insert(name.to_string(), now);
        }
        write_profile_cache(name, PROFILE_FETCHED_CACHE_FILE, &now);
    }
    want
}

/// Map an already-parsed `/profile` body to a [`PlanInfo`] (tier + raw
/// subscription status). The single place the fetch path turns `/profile` into a
/// plan, shared by the normal leg and the 429-bail leg.
fn plan_from_profile(p: &RawProfile) -> PlanInfo {
    let org = p.organization.as_ref();
    PlanInfo {
        tier: PlanTier::from_profile(
            org.and_then(|o| o.organization_type.as_deref()),
            p.account.as_ref().is_some_and(|a| a.has_claude_max),
            p.account.as_ref().is_some_and(|a| a.has_claude_pro),
            org.and_then(|o| o.rate_limit_tier.as_deref()),
        ),
        subscription_status: org.and_then(|o| o.subscription_status.clone()),
    }
}

/// The TTL-gated `/profile` leg on its own: `Some(plan)` only when
/// [`take_profile_fetch`] elects to fetch AND the leg parses; `None` when it's
/// skipped this round or the fetch/parse fails. Split out of [`fetch_raw`] so
/// the `/usage` 429 bail can run the SAME leg — a canceled account never returns
/// a 200 `/usage`, so this is the only path that ever sees its cancellation.
/// Never bypasses the hourly cap unless `force_profile` (the rotation retry
/// holding no plan yet).
fn fetch_profile_plan(
    name: &str,
    access_token: &str,
    force_profile: bool,
    activity: Option<&ActivityStore>,
) -> Option<PlanInfo> {
    if !take_profile_fetch(name, force_profile, now_ms()) {
        return None;
    }
    let text = get_json(
        PROFILE_ENDPOINT,
        access_token,
        activity,
        name,
        AuthClient::Profile,
    )
    .ok()?;
    let p: RawProfile = serde_json::from_str(&text).ok()?;
    seed_identity_anchor(name, &p);
    Some(plan_from_profile(&p))
}

/// Combine the `/usage` result with the `/profile` leg. On a 200 `/usage`,
/// behaves as before — a freshly fetched plan falls back to `prev_plan`. On a
/// 429, still runs `fetch_plan` and returns the error CARRYING only a freshly
/// observed plan (never `prev_plan`), so the scheduler persists the tier flip
/// exactly on the ~hourly tick `/profile` is re-pulled, not on every masked
/// tick. Split from the HTTP legs so the decouple is testable without live IO.
fn assemble_usage(
    usage: std::result::Result<String, FetchError>,
    prev_plan: Option<PlanInfo>,
    fetch_plan: impl FnOnce() -> Option<PlanInfo>,
) -> std::result::Result<UsageInfo, FetchError> {
    match usage {
        Ok(text) => {
            let raw: RawUsage = serde_json::from_str(&text).map_err(|_| FetchError::Parse)?;
            // A `/profile` failure never drops usage — it falls back to `prev_plan`.
            let plan = fetch_plan().or(prev_plan);
            let windows = windows_from_raw(&raw);
            let spend = raw.spend.as_ref().map(SpendInfo::from_raw);
            Ok(UsageInfo {
                plan,
                five_hour: windows.five_hour,
                seven_day: windows.seven_day,
                weekly_scoped: windows.weekly_scoped,
                window_dollars: windows.window_dollars,
                extra_usage: raw.extra_usage,
                spend,
                // Anthropic leg: the codex passive reader is the only writer.
                codex_rate_limit_reached: None,
            })
        }
        Err(FetchError::RateLimited { retry_after, .. }) => Err(FetchError::RateLimited {
            retry_after,
            plan: fetch_plan(),
        }),
        Err(e) => Err(e),
    }
}

/// Fetch `/usage`; fetch `/profile` only when [`take_profile_fetch`] says so,
/// otherwise carry `prev_plan` forward. `force_profile` bypasses the TTL; the
/// rotation retry sets it only when no plan is held yet, since a refresh mints a
/// token for the same account and can't change what `/profile` would say. A
/// `/usage` 429 no longer suppresses `/profile`: the profile leg still runs and
/// its plan rides the error, so a canceled (`claude_free`) account — which 429s
/// `/usage` on every tick — is finally observed.
pub(super) fn fetch_raw(
    name: &str,
    access_token: &str,
    prev_plan: Option<PlanInfo>,
    force_profile: bool,
    activity: Option<&ActivityStore>,
) -> std::result::Result<UsageInfo, FetchError> {
    let usage = get_json(
        USAGE_ENDPOINT,
        access_token,
        activity,
        name,
        AuthClient::Usage,
    );
    assemble_usage(usage, prev_plan, || {
        fetch_profile_plan(name, access_token, force_profile, activity)
    })
}

/// Backfill the profile's identity anchor (`account_id.json`) from an already-
/// parsed `/profile` response, riding the hourly tier fetch — zero extra HTTP.
/// A profile that predates login-time anchor seeding has none, and without one
/// `oauth::try_adopt_live_rotation` cannot prove a diverged live login is the
/// same account once the stored pair is fully dead — the profile wedges in
/// `auth_broken` even when the live session holds a healthy fresher pair
/// (observed 2026-07-09). Write-if-missing only: `clauth login` remains the
/// authoritative (re)seeder, and a blank uuid is shape drift, never an
/// identity (same contract as [`fetch_account_uuid`]).
///
/// The missing-check → write pair is deliberately not atomic: the only bad
/// interleave (a concurrent re-login to a DIFFERENT account landing its
/// anchor in that microsecond gap, then being overwritten by this ride-along)
/// fails SAFE — a wrong anchor only makes adoption refuse and self-heals on
/// the next login/adopt, so a cross-process lock isn't worth its weight here.
fn seed_identity_anchor(name: &str, profile: &RawProfile) {
    let Some(uuid) = profile
        .account
        .as_ref()
        .and_then(|a| a.uuid.as_deref())
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return;
    };
    let cached_uuid = load_profile_cache::<String>(name, ACCOUNT_ID_CACHE_FILE);
    if cached_uuid.is_none() {
        // An email surviving WITHOUT its uuid half predates this anchor (a
        // torn drop) and may describe the previous account — drop it before
        // seeding, so the pair is rebuilt whole from this one response.
        crate::profile_cache::drop_cache_file(name, crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE);
        write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &uuid.to_string());
    }
    // The email half rides the same response, same write-if-missing rule —
    // but ONLY when the uuid half agrees with this response (absent = just
    // seeded above). A cached uuid that differs means the anchor deliberately
    // still describes another account than the store (the CAP guard's
    // refuse-and-heal state); seeding a fresh email under it would split the
    // pair across two accounts.
    let uuid_agrees = cached_uuid.as_deref().is_none_or(|c| c.trim() == uuid);
    if uuid_agrees
        && let Some(email) = profile
            .account
            .as_ref()
            .and_then(|a| a.email.as_deref())
            .map(str::trim)
            .filter(|e| !e.is_empty())
        && load_profile_cache::<String>(name, ACCOUNT_EMAIL_CACHE_FILE).is_none()
    {
        write_profile_cache(name, ACCOUNT_EMAIL_CACHE_FILE, &email.to_string());
    }
}

/// Everything a login needs from one `/profile` body: the subscription-type
/// string Claude Code stores (`"max"`/`"pro"`/`"team"`/`"enterprise"`/`"free"`;
/// `None` for an unrecognized tier) and the account uuid the token authenticates
/// as. Either field is independently `None` — a body carrying one but not the
/// other still yields what it has.
pub(crate) struct LoginProfile {
    pub(crate) subscription_type: Option<String>,
    pub(crate) account_uuid: Option<String>,
}

/// Pull both login values out of an already-parsed `/profile` response. Split
/// from the HTTP leg so the mapping is testable against literal bodies.
/// A present-but-blank uuid is shape drift, never an identity (same contract as
/// [`fetch_account_uuid`]).
fn login_profile_from_raw(p: RawProfile) -> LoginProfile {
    let tier = {
        let org = p.organization.as_ref();
        PlanTier::from_profile(
            org.and_then(|o| o.organization_type.as_deref()),
            p.account.as_ref().is_some_and(|a| a.has_claude_max),
            p.account.as_ref().is_some_and(|a| a.has_claude_pro),
            org.and_then(|o| o.rate_limit_tier.as_deref()),
        )
    };
    LoginProfile {
        subscription_type: match tier {
            PlanTier::Max(_) => Some("max".to_string()),
            PlanTier::Pro => Some("pro".to_string()),
            PlanTier::Team => Some("team".to_string()),
            PlanTier::Enterprise => Some("enterprise".to_string()),
            PlanTier::Free => Some("free".to_string()),
            PlanTier::Unknown => None,
        },
        account_uuid: p
            .account
            .and_then(|a| a.uuid)
            .filter(|u| !u.trim().is_empty()),
    }
}

/// Fetch `/profile` ONCE with a freshly minted OAuth access token and read every
/// value the login needs out of that single body. Used by the interactive login
/// (`oauth_login`) to (a) confirm the minted token actually works against the API
/// — a `401` here means the login produced a dud token — (b) stamp the new
/// profile's tier so it shows the real plan immediately instead of the
/// unknown-tier "Pro" fallback, and (c) seed the identity anchor
/// ([`seed_login_anchor`]) without a second round trip. Goes through the shared
/// `/profile` fetch ([`AuthClient::Profile`]). Returns the HTTP error text so the
/// caller can surface it.
pub(crate) fn probe_login_profile(access_token: &str) -> anyhow::Result<LoginProfile> {
    let text = get_json(
        PROFILE_ENDPOINT,
        access_token,
        None,
        "login",
        AuthClient::Profile,
    )
    .map_err(|e| match e {
        FetchError::Status(s) => anyhow::anyhow!("profile endpoint returned HTTP {s}"),
        FetchError::RateLimited { .. } => anyhow::anyhow!("profile endpoint rate-limited (429)"),
        FetchError::Network => anyhow::anyhow!("network error reaching the profile endpoint"),
        FetchError::Parse => anyhow::anyhow!("profile response was not readable"),
    })?;
    let p: RawProfile = serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("profile response was not JSON"))?;
    Ok(login_profile_from_raw(p))
}

/// Seed a profile's identity anchor from a completed `clauth login`. UNCONDITIONAL
/// overwrite, unlike [`seed_identity_anchor`]'s write-if-missing ride-along: this
/// is the authoritative (re)seeder, so a reauth that swaps a DIFFERENT account
/// onto the name must replace the old anchor rather than keep proving the old
/// identity. Best-effort and silent on an absent/blank uuid (a failed probe or
/// shape drift) — a login is never failed over its anchor.
pub(crate) fn seed_login_anchor(name: &str, account_uuid: Option<&str>) {
    let Some(uuid) = account_uuid.map(str::trim).filter(|u| !u.is_empty()) else {
        return;
    };
    write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &uuid.to_string());
}

/// The account identity a freshly minted token authenticates as: the uuid
/// (machine half — what the adopt gate and same-account tripwire compare) and
/// the email when the response carries one (operator half — what the UIs show
/// and error messages name).
#[derive(Debug, Clone)]
pub(crate) struct AccountIdentity {
    pub(crate) uuid: String,
    pub(crate) email: Option<String>,
}

/// Outcome of asking `/api/oauth/profile` who an access token belongs to,
/// split so a caller can tell "the endpoint rejected the token itself" from
/// "the probe proves nothing". [`fetch_account_identity`]'s `Option` collapses
/// the two — fine for adopt gates (either way: identity unproven, refuse) but
/// not for the daemon's dead-login rescue (RESCUE-1), which may only reclaim
/// the live slot when the endpoint CONFIRMS the login is dead.
#[derive(Debug)]
pub(crate) enum IdentityProbe {
    /// The token authenticates as this account.
    Proven(AccountIdentity),
    /// The endpoint rejected the token itself (401): it is dead. A 403 stays
    /// [`IdentityProbe::Indeterminate`] — a WAF/geo block answers 403 for live
    /// tokens too (the same caution as `oauth::refresh_rejection_is_terminal`).
    Rejected,
    /// Transport failure, throttle, 5xx, or a response whose shape carried no
    /// identity — proves nothing about the token either way.
    Indeterminate,
}

/// The account identity `access_token` authenticates as, via
/// `/api/oauth/profile`, preserving the rejected-vs-unproven split.
pub(crate) fn probe_account_identity(access_token: &str) -> IdentityProbe {
    classify_identity_response(get_json(
        PROFILE_ENDPOINT,
        access_token,
        None,
        "identity",
        AuthClient::Profile,
    ))
}

/// [`probe_account_identity`]'s classification, pure so its truth table is
/// pinned offline (`identity_probe_classification_truth_table`) — this split
/// is the reclaim-vs-retry safety hinge of the dead-login rescue, and a drift
/// that mapped any transient failure to `Rejected` would point the destructive
/// reclaim at a login that is merely unreachable. ONLY a 401 rejects; a
/// present-but-blank uuid is shape drift, not an identity — two blanks
/// comparing equal must never prove two tokens are the same account.
fn classify_identity_response(response: std::result::Result<String, FetchError>) -> IdentityProbe {
    let text = match response {
        Ok(text) => text,
        Err(FetchError::Status(401)) => return IdentityProbe::Rejected,
        Err(_) => return IdentityProbe::Indeterminate,
    };
    let Some(account) = serde_json::from_str::<RawProfile>(&text)
        .ok()
        .and_then(|p| p.account)
    else {
        return IdentityProbe::Indeterminate;
    };
    let Some(uuid) = account
        .uuid
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
    else {
        return IdentityProbe::Indeterminate;
    };
    let email = account
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    IdentityProbe::Proven(AccountIdentity { uuid, email })
}

/// [`probe_account_identity`] collapsed to the adopt-gate shape: best-effort
/// `None` on any failure (network, 401, shape drift) — callers must treat that
/// as "identity unproven" and refuse.
pub(crate) fn fetch_account_identity(access_token: &str) -> Option<AccountIdentity> {
    match probe_account_identity(access_token) {
        IdentityProbe::Proven(id) => Some(id),
        IdentityProbe::Rejected | IdentityProbe::Indeterminate => None,
    }
}

/// [`fetch_account_identity`] narrowed to the uuid — the adopt-gate callers'
/// shape (`oauth::try_adopt_live_rotation` closures).
pub(crate) fn fetch_account_uuid(access_token: &str) -> Option<String> {
    fetch_account_identity(access_token).map(|id| id.uuid)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True iff `info`'s 5h usage window is still open — its reset time is in the
/// future at `now_secs`. A windowless or unparseable snapshot is not live.
pub(crate) fn five_hour_live(info: &UsageInfo, now_secs: i64) -> bool {
    info.five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs)
        .is_some_and(|resets_at| now_secs < resets_at)
}

/// [`five_hour_live`] for the OVERALL weekly window: live iff it carries a
/// parseable reset that is still in the future.
pub(crate) fn seven_day_live(info: &UsageInfo, now_secs: i64) -> bool {
    info.seven_day
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs)
        .is_some_and(|resets_at| now_secs < resets_at)
}

/// Seconds until a live window pinned at the 100% cap resets, or `None` when it
/// is below the cap, lapsed, or absent — the per-window primitive shared by
/// [`windows_maxed`] and [`spent_resume_in_secs`] so the two never drift. `live`
/// is the caller's already-computed liveness (guarantees a future, parseable
/// `resets_at`), so the re-parse here is total.
fn maxed_window_reset_in(live: bool, window: Option<&UsageWindow>, now_secs: i64) -> Option<i64> {
    let window = window?;
    if !live || window.utilization < 100.0 {
        return None;
    }
    let resets_at = window.resets_at.as_deref().and_then(iso_to_epoch_secs)?;
    Some(resets_at - now_secs)
}

/// True iff a request would currently be REFUSED: a live 5h **or** 7d window
/// pinned at the API's 100% cap. Such a window can't change until it resets, so
/// the opt-out `refresh_spent_accounts` fetch gate may skip the account until
/// then. Deliberately keyed on the 100% hard cap, NOT the sub-100 fallback
/// switch threshold: a below-cap window still moves and must keep being polled,
/// and this predicate must never influence switch/fallback decisions.
pub(crate) fn windows_maxed(info: &UsageInfo, now_secs: i64) -> bool {
    maxed_window_reset_in(
        five_hour_live(info, now_secs),
        info.five_hour.as_ref(),
        now_secs,
    )
    .is_some()
        || maxed_window_reset_in(
            seven_day_live(info, now_secs),
            info.seven_day.as_ref(),
            now_secs,
        )
        .is_some()
}

/// Seconds until a spent account (`windows_maxed`) resumes polling: the LATEST
/// reset among its live-maxed 5h/7d windows. It stays blocked until every maxed
/// window lapses, so a maxed weekly (7d) window dominates a maxed 5h one — the
/// caption reads "resets in <weekly>", not the sooner-but-still-blocked 5h.
/// `None` when the account is not currently maxed.
pub(crate) fn spent_resume_in_secs(info: &UsageInfo, now_secs: i64) -> Option<i64> {
    let five = maxed_window_reset_in(
        five_hour_live(info, now_secs),
        info.five_hour.as_ref(),
        now_secs,
    );
    let seven = maxed_window_reset_in(
        seven_day_live(info, now_secs),
        info.seven_day.as_ref(),
        now_secs,
    );
    match (five, seven) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Parse ISO-8601 timestamp (e.g. `2026-05-17T14:20:00.121699+00:00`) into Unix epoch seconds.
pub(crate) fn iso_to_epoch_secs(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: i64 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: i64 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    let tail = &s[19..];
    let after_frac = if let Some(rest) = tail.strip_prefix('.') {
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        &rest[end..]
    } else {
        tail
    };
    let tz_offset_secs: i64 = if after_frac.is_empty() || after_frac.starts_with('Z') {
        0
    } else {
        let sign = match after_frac.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        // Accept `±HH`, `±HHMM`, `±HH:MM`.
        let digits: String = after_frac[1..].chars().filter(|&c| c != ':').collect();
        if after_frac[1..]
            .chars()
            .any(|c| c != ':' && !c.is_ascii_digit())
        {
            return None;
        }
        let (tz_h, tz_m): (i64, i64) = match digits.len() {
            2 => (digits.parse().ok()?, 0),
            4 => (digits[0..2].parse().ok()?, digits[2..4].parse().ok()?),
            _ => return None,
        };
        sign * (tz_h * 3600 + tz_m * 60)
    };

    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + minute * 60 + second - tz_offset_secs)
}

/// Howard Hinnant's days-from-civil: days since 1970-01-01 for a proleptic
/// Gregorian `(year, month, day)`. Shared by the ISO-8601 and HTTP-date parsers.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Format Unix epoch seconds as ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS+00:00`) —
/// the shape [`iso_to_epoch_secs`] parses. Negative inputs clamp to epoch 0.
pub(crate) fn epoch_secs_to_iso(secs: i64) -> String {
    let secs = secs.max(0);
    let s = secs % 60;
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Civil-from-days — the inverse of days-from-civil in `iso_to_epoch_secs`.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

pub(crate) fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format seconds as `Nd Nh`, `Nh Nm`, `Nm`, `Nm Ns`, or `Ns`; returns `"now"`
/// for ≤0. Spans under 5 min carry seconds so an imminent countdown (a switch,
/// a kick lift, a reset) reads precisely instead of rounding up to a coarse
/// `1m`; whole minutes there still drop the trailing ` 0s`.
pub(crate) fn humanize_duration(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_string();
    }
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins % 60)
    } else if secs < 300 {
        match (mins, secs % 60) {
            (0, s) => format!("{s}s"),
            (m, 0) => format!("{m}m"),
            (m, s) => format!("{m}m {s}s"),
        }
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
#[path = "../../tests/inline/fetch.rs"]
mod tests;
