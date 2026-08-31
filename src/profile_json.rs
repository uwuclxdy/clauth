//! Profile → JSON view helpers shared by the `mcp` server, the `daemon`
//! status writer, and `clauth status --json`. Every reader sources usage from
//! the on-disk cache the scheduler writes — `usage_cache.json` for an OAuth
//! account and `third_party_cache.json` for an api-key one, picked by
//! [`usage_cache_file`] — so these functions are process-independent: they
//! return the last-persisted numbers whether or not a scheduler is live. One
//! home for the shape keeps the three surfaces from drifting.

use serde::{Deserialize, Serialize};

use crate::profile::{Profile, ProfileName};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::providers::{Provider, ThirdPartyStats};
use crate::usage::{PlanInfo, PlanTier, UsageInfo, now_ms};

/// The last-persisted `/profile` plan for a name, off the same on-disk cache
/// every reader here sources from.
fn cached_plan(name: &ProfileName) -> Option<PlanInfo> {
    load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE).and_then(|u| u.plan)
}

/// Cancellation for a surface holding a `load_config` profile. Deliberately NOT
/// [`crate::fallback::is_canceled`], which reads the in-memory `Profile::usage`
/// that only the TUI ever fills — outside it that predicate answers `false` for
/// every account, canceled or not. This reads the disk instead, so a CLI
/// surface gets the same answer the TUI does.
pub(crate) fn is_canceled_cached(name: &ProfileName) -> bool {
    cached_plan(name).is_some_and(|p| p.is_canceled())
}

/// Display provider for a profile, one of three cases: a recognised
/// provider's name, `"anthropic"` for a profile with no endpoint of its own,
/// `"generic"` for every other endpoint (owner ruling 2026-08-31). The OAuth
/// test is [`Profile::is_oauth`] — the managed `base_url` field alone, so the
/// label can never contradict the `base_url` it publishes beside, which was
/// the defect shape (`"anthropic"` next to a litellm URL). An operator-authored
/// `ANTHROPIC_BASE_URL` reroutes requests without retyping the account, the
/// same managed-field-only rule [`crate::profile::stored_provider`] applies.
pub(crate) fn provider_label(profile: &Profile) -> String {
    match profile.provider {
        Some(p) => p.display_name().to_string(),
        None if profile.is_oauth() => "anthropic".to_string(),
        None => "generic".to_string(),
    }
}

/// Human account-tier label for an OAuth profile, preferring the fetched plan
/// tier (carries the Max multiplier, e.g. `Max 5x`) over the bare OAuth
/// `subscription_type` token (`max`). Read straight off the on-disk `/profile`
/// cache, so it holds even before this session's first live fetch. `None` for
/// third-party/api-key profiles and when neither a fetched plan nor a token hint
/// is on disk.
///
/// Cancellation is a STATUS, not a tier: the org drops to `claude_free` when a
/// subscription is canceled, so a `Free` reading already carries it, and the
/// marker belongs on the status line the way every other surface renders it.
pub(crate) fn tier_label(profile: &Profile) -> Option<String> {
    if profile.is_third_party() {
        return None;
    }
    let fetched = cached_plan(&profile.name).filter(|p| p.tier != PlanTier::Unknown);
    match fetched {
        Some(plan) => plan.tier.short_label(),
        None => {
            let sub = profile
                .credentials
                .as_ref()?
                .claude_ai_oauth
                .as_ref()?
                .subscription_type
                .as_deref()?;
            PlanTier::from_subscription_type(Some(sub)).short_label()
        }
    }
}

/// The usage cache a profile's OWN fetch leg writes. The third-party leg never
/// touches `usage_cache.json`, so keying an api-key profile on it renders a
/// healthy hourly-refreshed account as never-fetched.
///
/// One selector rather than one per reader: that defect was found and fixed in
/// the daemon's status feed, then re-appeared verbatim in the MCP digest, which
/// is what a second copy of the rule buys. It asks
/// [`Profile::usage_cache_is_third_party`] — the question about where figures
/// live — never `is_third_party`, which answers whether the provider is one
/// clauth has a typed integration for and leaves every generic api-key endpoint
/// reading its empty OAuth cache.
pub(crate) fn usage_cache_file(p: &Profile) -> &'static str {
    cache_file_of(p.usage_cache_is_third_party())
}

/// [`usage_cache_file`] for a caller holding only a name, resolved through the
/// side-effect-free `stored_usage_cache_is_third_party` rather than a full
/// profile load: one caller samples this under a leaf lock at 5 Hz, where
/// recovering a staged rotation would take the state flock and invert the lock
/// order.
pub(crate) fn usage_cache_file_for(name: &ProfileName) -> &'static str {
    cache_file_of(crate::profile::stored_usage_cache_is_third_party(name))
}

fn cache_file_of(third_party: bool) -> &'static str {
    if third_party {
        THIRD_PARTY_CACHE_FILE
    } else {
        USAGE_CACHE_FILE
    }
}

/// The longest gap between two cache writes a LIVE scheduler can legally leave:
/// `partition_due` schedules the next poll at `last + interval + backoff`, where
/// the interval is capped by [`crate::profile::MAX_REFRESH_INTERVAL_MS`]
/// (3_600_000) and the widen-only backoff by
/// [`crate::usage::MAX_RETRY_AFTER_MS`] (900_000), so 4_500_000 ms.
const MAX_LIVE_REFRESH_GAP_MS: u64 =
    crate::profile::MAX_REFRESH_INTERVAL_MS + crate::usage::MAX_RETRY_AFTER_MS;

/// A cached figure older than this is not a reading anyone is maintaining — the
/// case a daemonless surface (the MCP server runs no scheduler by design) hits
/// by default. Twice [`MAX_LIVE_REFRESH_GAP_MS`], because that gap is measured
/// between SLOTS while this is measured between WRITES: one fetch's own latency
/// and one tick of partition granularity both land on top of it, and at the
/// ceiling interval either alone would make a healthy account read stale.
const STALE_AFTER_MS: u64 = 2 * MAX_LIVE_REFRESH_GAP_MS;

/// What clauth can say about one account's headroom, discriminated so a reader
/// can tell a window that does not EXIST from a window with no cached figure.
///
/// Each arm carries the age of the cache its own figures came from, because the
/// file that answers is the file that dates the answer: reading figures out of
/// one cache and their freshness out of the other is the defect this type makes
/// unspellable. A stale figure is DATED, never dropped — a known-old number a
/// reader can discount beats no number, which reads as clauth having lost track
/// of the account.
pub(crate) enum ProfileWindows {
    /// An OAuth account's own `/usage` read. `None` when nothing has been
    /// fetched yet, which is a missing FIGURE rather than a missing window.
    /// Boxed: a `UsageInfo` is ~464 bytes against the third-party arm's 120, and
    /// this type is returned by value on every reply.
    Oauth {
        usage: Option<Box<UsageInfo>>,
        age_secs: Option<u64>,
    },
    /// A third-party account. The 5h/7d pool is not this account's pool at all,
    /// so that window is structurally none; its provider's own cached stats are
    /// what it publishes instead, `None` until that leg first writes them.
    ThirdParty {
        stats: Option<ThirdPartyStats>,
        age_secs: Option<u64>,
        /// The recognised provider, so the prose can decide the 5h/7d denial
        /// from what the PROVIDER publishes rather than from what one response
        /// carried. `None` for a generic endpoint.
        provider: Option<Provider>,
    },
}

impl ProfileWindows {
    /// How long ago the cache behind these figures was written. `None` when
    /// there is no cache, which is also when there are no figures to date.
    pub(crate) fn age_secs(&self) -> Option<u64> {
        match self {
            Self::Oauth { age_secs, .. } | Self::ThirdParty { age_secs, .. } => *age_secs,
        }
    }

    /// Whether these figures are past [`STALE_AFTER_MS`].
    pub(crate) fn stale(&self) -> bool {
        self.age_secs()
            .is_some_and(|age| age.saturating_mul(1000) > STALE_AFTER_MS)
    }
}

/// Read one account's headroom out of whichever cache its own fetch leg writes,
/// discriminated by [`ProfileWindows`].
pub(crate) fn profile_windows(p: &Profile) -> ProfileWindows {
    windows_of(&p.name, p.usage_cache_is_third_party(), p.provider)
}

/// [`profile_windows`] for a caller holding only a name, classified the same
/// side-effect-free way [`usage_cache_file_for`] classifies its own.
pub(crate) fn profile_windows_for(name: &ProfileName) -> ProfileWindows {
    windows_of(
        name,
        crate::profile::stored_usage_cache_is_third_party(name),
        crate::profile::stored_provider(name),
    )
}

fn windows_of(name: &ProfileName, third_party: bool, provider: Option<Provider>) -> ProfileWindows {
    let file = cache_file_of(third_party);
    let age_secs = cache_age_secs(name, file);
    if third_party {
        return ProfileWindows::ThirdParty {
            stats: load_profile_cache::<ThirdPartyStats>(name, file),
            age_secs,
            provider,
        };
    }
    ProfileWindows::Oauth {
        usage: load_profile_cache::<UsageInfo>(name, file).map(Box::new),
        age_secs,
    }
}

/// Seconds since `file` was last written for `name`; `None` when it is absent,
/// and `None` again when its stamp is in the FUTURE. A saturating subtraction
/// would render that as `cached just now` with `stale` false — maximum
/// confidence for the one stamp that proves the clock is wrong — where an
/// undated figure says exactly what clauth knows: it cannot date this one.
fn cache_age_secs(name: &ProfileName, file: &str) -> Option<u64> {
    let mtime = profile_cache_mtime_ms(name, file)?;
    now_ms().checked_sub(mtime).map(|age| age / 1000)
}

/// One published OAuth window row — the `{label, utilization_pct, resets_at}`
/// spelling of a 5h, 7d, or per-model weekly window. Both writers
/// ([`oauth_windows`] → the daemon's `status.json` feed and the MCP payloads)
/// and the reader (`clauth list`'s 5h/7d columns) derive from this one struct,
/// so a reader's key spelling cannot drift from what a writer emits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Window {
    pub(crate) label: String,
    pub(crate) utilization_pct: f64,
    pub(crate) resets_at: Option<String>,
}

/// The [`Window`] rows of an OAuth usage read — 5h, 7d, then one entry per
/// weekly model window (`7d <model>`).
pub(crate) fn oauth_windows(usage: &UsageInfo) -> Vec<Window> {
    usage
        .windows()
        .into_iter()
        .map(|(label, w)| Window {
            label: label.to_string(),
            utilization_pct: w.utilization,
            resets_at: w.resets_at.clone(),
        })
        .collect()
}

/// The profile's OAuth usage windows, read fresh from the disk cache; empty
/// when there is no cache. The rows of the published `status.json` `windows`
/// array — hence this flat spelling rather than [`ProfileWindows`]'s
/// discriminated one, which the MCP surface renders.
///
/// Empty TOO for an account whose figures live in the third-party cache, the
/// shared cache selector's name-only form
/// ([`crate::profile::stored_usage_cache_is_third_party`]): whatever
/// `usage_cache.json` still holds for one is a leftover from an earlier OAuth
/// life, and publishing it rendered a stale 100% Anthropic window beside
/// `"third_party":{"available":true}` for an account with no Anthropic window.
pub(crate) fn published_windows(name: &ProfileName) -> Vec<Window> {
    if crate::profile::stored_usage_cache_is_third_party(name) {
        return Vec::new();
    }
    load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
        .as_ref()
        .map(oauth_windows)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../tests/inline/profile_json.rs"]
mod tests;
