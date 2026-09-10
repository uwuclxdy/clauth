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
use crate::usage::{PlanInfo, PlanTier, UsageInfo, UsageWindow, now_ms};

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
    if profile.usage_cache_is_third_party() {
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

/// The longest gap between two cache writes a LIVE scheduler can legally leave.
/// The widen-only backoff is either the #74 degraded floor — a non-hint
/// deferral clamps the total gap to `max(interval, 5min)`, so zero extra at
/// the ceiling interval — or a server `retry-after` capped at
/// [`crate::usage::MAX_RETRY_AFTER_MS`] (900_000). The wider of the two over
/// every interval is the ceiling interval itself: 3_600_000 ms. (Plus the
/// per-fetch `deadline_spread`, up to `interval/4`; the 2× margin below
/// absorbs it.)
const MAX_LIVE_REFRESH_GAP_MS: u64 =
    if crate::profile::MAX_REFRESH_INTERVAL_MS > crate::usage::MAX_RETRY_AFTER_MS {
        crate::profile::MAX_REFRESH_INTERVAL_MS
    } else {
        crate::usage::MAX_RETRY_AFTER_MS
    };

/// A cached figure older than this is not a reading anyone is maintaining — the
/// case a daemonless surface (the MCP server runs no scheduler by design) hits
/// by default. Twice [`MAX_LIVE_REFRESH_GAP_MS`], because that gap is measured
/// between SLOTS while this is measured between WRITES: one fetch's own latency
/// and one tick of partition granularity both land on top of it, and at the
/// ceiling interval either alone would make a healthy account read stale.
const STALE_AFTER_MS: u64 = 2 * MAX_LIVE_REFRESH_GAP_MS;

/// Cache age past which a reading is stale, derived from the cadence a reader
/// polls at: `2 × max(interval_ms, 5min) + interval_ms`. The home for the
/// `status.json` age arm and any reader that polls at a known interval. The
/// 5-minute floor is the ceiling the degraded-fetch cadence work (#74) clamps
/// every backoff ladder to. `interval_ms` is the LIVE refresh interval the
/// caller polls at, so a deliberately slow cadence widens the grace rather than
/// redding it. [`ProfileWindows::stale`] instead reads the fixed
/// [`STALE_AFTER_MS`]: the MCP server runs no scheduler, holds no interval to
/// derive from, and the widest-interval derivation is the honest ceiling for
/// it.
pub(crate) fn stale_after_ms(interval_ms: u64) -> u64 {
    let floored = interval_ms.max(crate::usage::DEGRADED_GAP_CEILING_MS);
    2 * floored + interval_ms
}

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
        age: OauthAge,
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

/// How old an OAuth account's cached figures are, and whether anything dates
/// them. The one age contract every OAuth surface reads: `status.json`, the TUI
/// stale cue and the MCP payloads all derive from this, so no two of them can
/// answer differently about the same file.
///
/// The stamp rides in the BODY ([`UsageInfo::fetched_at`], written only by a
/// live fetch outcome), never on the file. A plan-only cache rewrite touches the
/// mtime without producing a new reading, and dating off mtime let exactly that
/// rewrite re-age a figure nothing had re-fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OauthAge {
    /// No cache at all: nothing to date, and no figures to distrust.
    Absent,
    /// A cache with no usable stamp: missing (a plan-only cold fill, or a body
    /// written before the field existed) or dated in the FUTURE, which proves
    /// the clock moved rather than that the read is fresh. Its figures stay
    /// VISIBLE and read stale, because a figure of unknown age is the one a
    /// reader must discount hardest.
    Undated,
    /// Milliseconds since the fetch that produced these figures. Carried in
    /// the unit `is_stale` compares in, so the verdict flips on the exact
    /// threshold instant rather than up to a second late (the pre-R10 shape
    /// divided to seconds here and multiplied back there).
    Dated(u64),
}

impl OauthAge {
    /// The age a reader can publish. `None` for both arms that carry no trusted
    /// number, which is why staleness is a separate question.
    pub(crate) fn secs(self) -> Option<u64> {
        match self {
            Self::Dated(ms) => Some(ms / 1000),
            Self::Absent | Self::Undated => None,
        }
    }

    /// Whether the figures behind this age should be discounted. `Undated` is
    /// stale on its own: there is a cache, and nothing says when it was read.
    ///
    /// `has_figures` is [`publishes_a_live_window`]: a verdict qualifies a
    /// FIGURE, and a body publishing none is never stale, whatever its age
    /// (owner ruling 2026-09-09). A `(stale)` marker beside a dash tells a
    /// reader that a number they cannot see is old.
    pub(crate) fn is_stale(self, threshold_ms: u64, has_figures: bool) -> bool {
        if !has_figures {
            return false;
        }
        match self {
            Self::Absent => false,
            Self::Undated => true,
            Self::Dated(ms) => ms > threshold_ms,
        }
    }
}

/// Classify an OAuth account's cached body against `now_ms`. `usage` is what the
/// production reader returned, so an absent file and an unreadable one both
/// arrive here as `None`.
pub(crate) fn oauth_age(usage: Option<&UsageInfo>, now_ms: u64) -> OauthAge {
    let Some(usage) = usage else {
        return OauthAge::Absent;
    };
    match usage.fetched_at {
        Some(at) => now_ms
            .checked_sub(at)
            .map_or(OauthAge::Undated, OauthAge::Dated),
        None => OauthAge::Undated,
    }
}

impl ProfileWindows {
    /// How long ago the fetch behind these figures ran. `None` when nothing
    /// dates them, which is a separate question from [`Self::stale`].
    pub(crate) fn age_secs(&self) -> Option<u64> {
        match self {
            Self::Oauth { age, .. } => age.secs(),
            Self::ThirdParty { age_secs, .. } => *age_secs,
        }
    }

    /// Whether these figures are past [`STALE_AFTER_MS`], or carry no age to
    /// judge against it.
    pub(crate) fn stale(&self) -> bool {
        match self {
            Self::Oauth { age, usage } => age.is_stale(
                STALE_AFTER_MS,
                usage.as_ref().is_some_and(|u| publishes_a_live_window(u)),
            ),
            Self::ThirdParty { age_secs, .. } => {
                age_secs.is_some_and(|age| age.saturating_mul(1000) > STALE_AFTER_MS)
            }
        }
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
    if third_party {
        // The provider leg's only writer is a fetch outcome, so its file mtime
        // IS its read time and it keeps dating off the file.
        return ProfileWindows::ThirdParty {
            stats: load_profile_cache::<ThirdPartyStats>(name, file),
            age_secs: cache_age_secs(name, file),
            provider,
        };
    }
    let usage = load_profile_cache::<UsageInfo>(name, file);
    ProfileWindows::Oauth {
        age: oauth_age(usage.as_ref(), now_ms()),
        usage: usage.map(Box::new),
    }
}

/// Seconds since `file` was last written for `name`; `None` when it is absent,
/// and `None` again when its stamp is in the FUTURE. A saturating subtraction
/// would render that as `cached just now` with `stale` false — maximum
/// confidence for the one stamp that proves the clock is wrong — where an
/// undated figure says exactly what clauth knows: it cannot date this one.
pub(crate) fn cache_age_secs(name: &ProfileName, file: &str) -> Option<u64> {
    let mtime = profile_cache_mtime_ms(name, file)?;
    now_ms().checked_sub(mtime).map(|age| age / 1000)
}

/// One published window row — the `{label, utilization_pct, resets_at}`
/// spelling of a 5h, 7d, or per-model weekly window. Both writers
/// ([`usage_windows`] → the daemon's `status.json` feed and the MCP payloads)
/// and the reader (`clauth list`'s 5h/7d columns) derive from this one struct,
/// so a reader's key spelling cannot drift from what a writer emits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Window {
    pub(crate) label: String,
    pub(crate) utilization_pct: f64,
    pub(crate) resets_at: Option<String>,
}

/// Whether one window row is still a CURRENT reading: a parseable `resets_at`
/// in the future, or no parseable stamp at all. This is the row-level half of
/// the [`usage_windows`] drop (#74), shared with the MCP `5h/7d_used_pct`
/// fields and the roster's own rank, so one lapsed window cannot read as a
/// spent account through one surface while the row it came from drops through
/// another. Deliberately looser than [`crate::usage::five_hour_live`], which
/// requires a future stamp: that predicate decides whether a window EXISTS for
/// fetch-skip logic, while this one keeps the unstamped row the published
/// array already carries.
pub(crate) fn window_row_is_live(w: &UsageWindow) -> bool {
    w.resets_at
        .as_deref()
        .and_then(crate::usage::iso_to_epoch_secs)
        .is_none_or(|resets_at| crate::usage::now_epoch_secs() < resets_at)
}

/// Whether a body still publishes a window a reader can act on. Every surface
/// filters its rows through [`window_row_is_live`], so a body whose windows have
/// all lapsed renders dashes exactly like one that carries none.
///
/// This is the figure a staleness verdict qualifies (owner ruling 2026-09-09):
/// with no visible number there is nothing to discount, and a `(stale)` marker
/// beside a dash tells a reader that a figure they cannot see is old.
pub(crate) fn publishes_a_live_window(usage: &UsageInfo) -> bool {
    usage.windows().iter().any(|(_, w)| window_row_is_live(w))
}

/// [`window_row_is_live`] for a third-party provider's cached bar: the same
/// one-derivation liveness, shared with the roster's rank and headline so a
/// lapsed bar cannot rank or render its last utilization while the OAuth
/// window it mirrors drops (#74). The bar's own `resets_at` stamps it only
/// when the provider's response carried one (z.ai, generic, Alibaba); an
/// unstamped bar stays, the same missing-data call the OAuth row makes.
pub(crate) fn usage_bar_is_live(b: &crate::providers::UsageBar) -> bool {
    b.resets_at
        .as_deref()
        .and_then(crate::usage::iso_to_epoch_secs)
        .is_none_or(|resets_at| crate::usage::now_epoch_secs() < resets_at)
}

/// The [`Window`] rows of a usage read — 5h, 7d, then one entry per weekly
/// model window (`7d <model>`). Serves both cache shapes: an OAuth read
/// directly, and a third-party read through the derivation
/// [`published_windows`] maps it with. A window whose `resets_at` has passed
/// drops here (#74): past its reset the figure is the previous window's last
/// utilization, not a current reading, and a lapsed 5h at `100%` read as a
/// permanently spent account. A window with no parseable `resets_at` stays —
/// absence of a stamp is missing data, not a lapsed window, and the row
/// without it is the honest shape every pre-reset row already publishes.
pub(crate) fn usage_windows(usage: &UsageInfo) -> Vec<Window> {
    usage
        .windows()
        .into_iter()
        .filter(|(_, w)| window_row_is_live(w))
        .map(|(label, w)| Window {
            label: label.to_string(),
            utilization_pct: w.utilization,
            resets_at: w.resets_at.clone(),
        })
        .collect()
}

/// The headroom a THIRD-PARTY account's own cache holds, as the two figure
/// strings `clauth list`'s 5h/7d columns render: the provider's own usage bars
/// under those exact labels first (a LIVE bar's figures; a lapsed one is the
/// previous window's last reading and drops, the same call [`usage_windows`]
/// makes), falling back to the first FUNDED wallet's balance — the same
/// fall-through the MCP roster's rank uses, so a bar the roster ranks the
/// account on cannot render as dashes here and vice versa. A live bar under a
/// NON-canonical label (the generic scanner's provider-authored labels) is no
/// match: it neither fills a column nor suppresses the wallet fallback. The
/// OAuth figure gate stays OAuth-only (owner ruling 2026-09-09): a `Window`
/// newtype would let a wallet masquerade as a window, so the plain pair keeps
/// the two figure families from meeting.
///
/// `(None, None)` when the account has neither — the columns stay dashes, the
/// missing-data call every other surface makes.
pub(crate) fn third_party_columns(p: &Profile) -> (Option<String>, Option<String>) {
    let Some(stats) = load_profile_cache::<ThirdPartyStats>(&p.name, THIRD_PARTY_CACHE_FILE) else {
        return (None, None);
    };
    let bar = |label: &str| {
        stats
            .bars
            .iter()
            .find(|b| b.label == label && usage_bar_is_live(b))
            .map(|b| match (b.used, b.total) {
                // The account's own reported amounts outrank the derived
                // percentage the same bar carries.
                (Some(used), Some(total)) => format!(
                    "{} / {}",
                    crate::format::format_amount(used),
                    crate::format::format_amount(total)
                ),
                _ => crate::format::format_pct(b.pct),
            })
    };
    match (bar(crate::usage::LABEL_5H), bar(crate::usage::LABEL_7D)) {
        (None, None) => {
            // No label-matched live bar: the wallet row is the headroom a
            // scalar provider (or a non-canonically-labeled one) publishes.
            let wallet = crate::providers::funded_wallets(&stats.rows)
                .into_iter()
                .next()
                .map(|w| w.value);
            (wallet, None)
        }
        columns => columns,
    }
}

/// The profile's usage windows, read fresh from the disk cache; empty when
/// there is no cache. The rows of the published `status.json` `windows` array —
/// hence this flat spelling rather than [`ProfileWindows`]'s discriminated one,
/// which the MCP surface renders.
///
/// Each account is read from ITS OWN cache, on the shared cache selector's
/// name-only form ([`crate::profile::stored_usage_cache_is_third_party`]) — an
/// api-key account's windows come from `third_party_cache.json` via
/// [`ThirdPartyStats::to_usage_info`], never from `usage_cache.json`. Reading
/// the OAuth file for one published a stale 100% Anthropic window beside
/// `"third_party":{"available":true}`, a leftover from an earlier OAuth life
/// describing a window that account no longer has; keeping the two files apart
/// is what makes publishing the third-party figures safe rather than a second
/// way to render that same leftover.
pub(crate) fn published_windows(name: &ProfileName) -> Vec<Window> {
    if crate::profile::stored_usage_cache_is_third_party(name) {
        return load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
            .as_ref()
            .and_then(ThirdPartyStats::to_usage_info)
            .as_ref()
            .map(usage_windows)
            .unwrap_or_default();
    }
    load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
        .as_ref()
        .map(usage_windows)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../tests/inline/profile_json.rs"]
mod tests;
