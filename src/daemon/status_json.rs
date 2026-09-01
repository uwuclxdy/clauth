//! `~/.clauth/status.json` serializer — the daemon's published feed, and the
//! shape `clauth status --json` prints (one code path builds both, so they
//! cannot drift). Contract: wiki/Daemon.md.
//!
//! Usage windows/tier come from the on-disk `usage_cache.json` (written by the
//! scheduler), so this is process-independent: it returns the last-persisted
//! numbers whether or not a scheduler is live. Two fields — `fetch_status` and
//! `next_refresh_at` — live only in the scheduler's in-memory stores; when a
//! live daemon passes [`LiveSignals`] they come from there, otherwise they are
//! derived from the cache-file mtime so the single-shot `status --json` still
//! produces a coherent shape.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::profile::{AppConfig, Profile, ProfileName};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::profile_json::{
    Window, provider_label, published_windows, tier_label, usage_cache_file,
};
use crate::providers::ThirdPartyStats;
use crate::usage::{
    FetchStatus, UsageInfo, epoch_secs_to_iso, is_stuck_rate_limited, now_ms, windows_maxed,
};

/// Bump when the JSON shape changes in a way readers must branch on.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// Live scheduler signals a running daemon has that the single-shot
/// `clauth status --json` cannot see. When absent, freshness and next-refresh
/// are derived from the cache-file mtime instead.
///
/// These are already-snapshotted plain maps, not the live `Arc<RankedMutex<…>>`
/// stores. [`build_status`] runs holding NO lock at all — the config it takes is
/// a snapshot too — because it stats and reads every profile's caches and sweeps
/// the session flocks; a caller that held CONFIG (which outranks `USAGE_STATUS`)
/// across those reads would both invert lock order and stall every other config
/// user for the duration.
pub(crate) struct LiveSignals<'a> {
    pub(crate) status: &'a HashMap<String, FetchStatus>,
    /// The THIRD-PARTY leg's outcomes, kept as a separate map rather than merged
    /// into `status`: `stale` below is contracted as a stuck 429 read off the
    /// OAuth store, and folding the two would silently retarget it.
    pub(crate) third_party_status: &'a HashMap<String, FetchStatus>,
    pub(crate) next_refresh: &'a HashMap<String, u64>,
    /// Consecutive-429 streaks, so a profile whose live `status` is `RateLimited`
    /// AND whose streak has passed the active cap can be published as `stale` (a
    /// deep-slot stuck read the daemon distrusts — the same judgment
    /// `scan_auto_switch` acts on). Empty for the single-shot `status --json` (no
    /// daemon), so `stale` is always `false` there.
    pub(crate) streaks: &'a HashMap<String, u32>,
    /// The switch target the daemon has accepted but not yet applied (from
    /// `pending_switch`), so a reader can show in-flight truth instead of a
    /// timing heuristic. `None` for the single-shot `status --json` (no daemon).
    pub(crate) pending_switch: Option<&'a str>,
    /// The scheduler's in-memory auto-start queue anchor
    /// ([`crate::usage::queue_anchor_cached`]), so the published `next_open_at`
    /// matches the value the election gates on. `None` while nothing has
    /// opened — published as a `null` `next_open_at`, which reads as "due
    /// now". The single-shot `status --json` has no scheduler and derives it
    /// from the usage-history series instead ([`crate::usage::history_anchor`])
    /// — one replay per invocation, a cost the per-tick daemon feed must not
    /// pay.
    pub(crate) queue_anchor: Option<i64>,
    /// The scheduler's switch-grade kick blocks, as the names its own election
    /// excludes from the queue ([`crate::usage::auto_start_queue_members`]'s
    /// `blocked`). Carried here for the same reason as `queue_anchor`: the set
    /// lives in the scheduler's memory, this builder holds no lock, and a
    /// published queue that disagrees with the one being gated is the exact
    /// divergence the shared membership rule was extracted to prevent. The
    /// single-shot `status --json` has no scheduler and reads the same blocks
    /// off their `kick_block.json` caches instead
    /// ([`crate::usage::switch_grade_kick_blocked_from_cache`]).
    pub(crate) queue_blocked: &'a [ProfileName],
}

fn fetch_status_str(s: FetchStatus) -> &'static str {
    match s {
        FetchStatus::Fresh => "Fresh",
        FetchStatus::Cached => "Cached",
        FetchStatus::Failed => "Failed",
        FetchStatus::RateLimited => "RateLimited",
        FetchStatus::AuthExpired => "AuthExpired",
    }
}

/// ISO-8601 (UTC) from an epoch-millisecond instant.
fn iso_from_ms(ms: u64) -> String {
    epoch_secs_to_iso((ms / 1000) as i64)
}

/// The `fallback` object for a profile, or `None` when it is not a chain member.
/// `armed` = in the chain AND currently active (the account auto-switch would
/// rotate away from). `position` is 1-based.
fn fallback_json(config: &AppConfig, p: &Profile) -> Option<serde_json::Value> {
    let name = &p.name;
    let pos = config.state.fallback_chain.iter().position(|n| n == name)?;
    Some(serde_json::json!({
        "position": pos + 1,
        "threshold": crate::fallback::threshold_for(p),
        "armed": config.is_active(name),
    }))
}

/// Per-profile auth health for `status.json`. `broken` (last refresh rejected
/// as revoked/invalid — `AppState::auth_broken`) outranks `expiring` (an OAuth
/// access token past its expiry, refresh not yet run); everything else is
/// `ok`. Readers default an absent field to `ok` (the additive-evolution
/// rule); it is still emitted for an explicit, greppable contract.
///
/// Keyed on credential typing ([`Profile::login_is_oauth`]), not endpoint routing:
/// this reports on the token the profile STORES, and a hybrid (an OAuth pair plus
/// a `base_url`) holds one that expires like any other. Reading it behind the
/// endpoint gate published a permanent `ok` over a dead token. The value set is
/// unchanged, so the schema stays 1.
fn auth_status_str(config: &AppConfig, p: &Profile, now_ms: i64) -> &'static str {
    if config.is_auth_broken(&p.name) {
        return "broken";
    }
    if p.login_is_oauth() && p.access_token_expires_at().is_some_and(|exp| now_ms >= exp) {
        return "expiring";
    }
    "ok"
}

/// One profile's `auto_start_queue` object in `status.json`: the 1-based slot,
/// and the queue's shared next-open ESTIMATE — the queue gates globally, so the
/// stamp is when the NEXT window opens, whoever opens it, and a window opened
/// out of band moves it as soon as the gate takes that opening up
/// ([`crate::usage::queue_anchor`]). `next_open_at` is `null` only when no anchor is
/// derivable yet (cold history); an anchored-but-due queue publishes
/// `anchor + gap` even once that instant is past — readers compare it to now,
/// exactly as wiki/Daemon.md contracts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueEntry {
    pub(crate) position: usize,
    pub(crate) next_open_at: Option<String>,
}

/// One `profiles[]` entry of the published `status.json` body — the shape both
/// the writer ([`build_profile_entries`], serialized by [`build_status`]) and
/// the reader (`clauth list`'s table rows) derive from, so a reader's field
/// access cannot drift from what the writer emits. Contract: wiki/Daemon.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProfileEntry {
    pub(crate) name: ProfileName,
    /// Active-profile marker source. The active profile is always kept, disabled
    /// or not: the top-level `active_profile` field names it unconditionally,
    /// and a reader resolves that name against `profiles[]`.
    pub(crate) active: bool,
    /// Additive (CLA-ROLL): what the sidecar actually HOLDS — the same content
    /// classification the TUI renders, not the config flag. The two part ways
    /// exactly when honesty matters: a dead chain degrades the sidecar onto its
    /// static mint while the flag stays on, and the flag would promise readers
    /// a re-stamp for a mint nobody is going to re-stamp. While true, the
    /// sidecar's hours-scale countdown is routine maintenance (daemon re-stamps
    /// on rotation and on the freshness timer); while false it is a real
    /// credential clock. Readers key their token-row rendering off this so a
    /// rolling token never displays as an expiring mint — nor the reverse.
    pub(crate) rolling_token: bool,
    /// Display provider label: a recognised third-party name, else `anthropic`.
    pub(crate) provider: String,
    /// The third-party endpoint, `None` for the default Anthropic one.
    pub(crate) base_url: Option<String>,
    /// Human tier label for an anthropic account (`Max 5x`); `None` for
    /// third-party/api-key profiles.
    pub(crate) tier: Option<String>,
    /// A live `clauth start` session runs for this profile.
    pub(crate) has_live_session: bool,
    /// `ok` / `expiring` / `broken` (see [`auth_status_str`]).
    pub(crate) auth_status: String,
    /// Freshness: a live daemon's verdict or the cache-mtime derivation; `None`
    /// when there is no cache at all.
    pub(crate) fetch_status: Option<String>,
    /// Additive (schema stays 1): true when the daemon distrusts this reading
    /// as a deep-slot stuck RateLimited — readers dim it / show a "stuck" cue
    /// instead of treating it as current truth. Always false for the single-shot
    /// `status --json`.
    pub(crate) stale: bool,
    /// ISO-8601 UTC stamp of the cache behind the published figures; `None`
    /// when there is no cache.
    pub(crate) fetched_at: Option<String>,
    /// ISO-8601 UTC stamp of the next scheduled refresh; `None` when none is
    /// pending (a spent skipped account, or no cache).
    pub(crate) next_refresh_at: Option<String>,
    pub(crate) auto_start: bool,
    /// Additive (schema stays 1): this profile's slot in the interleaved
    /// auto-start queue, `None`/`null` when it holds none — the toggle is off,
    /// it never opted into `auto_start`, or it cannot open a window.
    /// `default` so a reader stays additive-tolerant of an older writer.
    #[serde(default)]
    pub(crate) auto_start_queue: Option<QueueEntry>,
    pub(crate) bell_threshold: Option<f64>,
    /// The chain-membership object (`position` / `threshold` / `armed`), `None`
    /// when not a chain member.
    pub(crate) fallback: Option<serde_json::Value>,
    /// The OAuth 5h/7d usage rows; empty when the profile has no OAuth cache.
    pub(crate) windows: Vec<Window>,
    /// The third-party availability object (`available`), `None` for OAuth
    /// accounts.
    pub(crate) third_party: Option<serde_json::Value>,
}

/// The per-profile entries [`build_status`] publishes — typed, so a reader
/// (`clauth list`) derives its fields instead of re-spelling string keys. One
/// builder for both surfaces, so they cannot drift.
///
/// `include_disabled` gates whether a user-disabled account appears in the
/// `profiles` array at all — the daemon's own `status.json` feed always passes
/// `false` (hidden by default); the single-shot `clauth status --json --all`/
/// `--disabled` flag flips it to `true`.
pub(crate) fn build_profile_entries(
    config: &AppConfig,
    interval_ms: u64,
    live: Option<&LiveSignals>,
    include_disabled: bool,
) -> Vec<ProfileEntry> {
    let now = now_ms();
    // Interleaved auto-start queue (`usage::auto_start_queue`), hoisted so the
    // membership and anchor are resolved once rather than per profile. Both
    // inputs come from the same places the scheduler's own election reads them,
    // so the published slot cannot disagree with the one being gated: a live
    // daemon passes its in-memory blocks and anchor through `LiveSignals`, and
    // the daemonless `status --json` re-derives each from disk — the
    // `kick_block.json` caches the scheduler writes through, and the
    // usage-history series. Both derivations are one pass per invocation, a
    // cost the per-tick daemon feed must not pay.
    let blocked = match live {
        Some(l) => l.queue_blocked.to_vec(),
        None => crate::usage::switch_grade_kick_blocked_from_cache(
            &config
                .profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            (now / 1000) as i64,
        ),
    };
    let queue_members = crate::usage::auto_start_queue_members(config, &blocked);
    let queue_anchor = match live {
        Some(l) => l.queue_anchor,
        // The anchor replays every profile's history, never just the queue
        // members' — the same full list the scheduler's own seed and per-tick
        // gate derive from, so this published anchor cannot disagree with the
        // one the election is gating on.
        None => crate::usage::history_anchor(
            &config
                .profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        ),
    };
    let next_queue_open =
        crate::usage::next_queue_open_secs(queue_anchor, queue_members.len(), interval_ms)
            .and_then(|s| u64::try_from(s.saturating_mul(1000)).ok());
    config
        .profiles
        .iter()
        .filter(|p| include_disabled || !p.is_disabled() || config.is_active(&p.name))
        .map(|p| {
            let name = &p.name;
            // Freshness reads each profile's OWN cache, through the one
            // selector every reader shares (`usage_cache_file` carries why).
            let mtime_ms = profile_cache_mtime_ms(name, usage_cache_file(p));

            // fetch_status: the live stores when a daemon is running, else
            // derive from cache freshness (Fresh within one interval, else
            // Cached). A name in NEITHER live store (a just-started daemon, the
            // single-shot `status --json`) falls back to that derivation rather
            // than reading as never-fetched; null = no cache at all.
            //
            // Both stores are consulted, OAuth first — the same precedence the
            // TUI's own merge applies, so the two surfaces can't disagree about
            // a hybrid. Reading `status` alone left every third-party outcome to
            // the mtime derivation, which can only ever say Fresh/Cached/null:
            // an `AuthExpired` session writes no cache and published `null`
            // (indistinguishable from never fetched), a 429 published a
            // freshness claim about a rejected poll, and an `AuthExpired` over a
            // stale cache published `Fresh` — a dead session reading as live,
            // which is the outcome this status exists to prevent.
            let derived_status = || {
                mtime_ms.map(|mt| {
                    if now.saturating_sub(mt) < interval_ms {
                        "Fresh"
                    } else {
                        "Cached"
                    }
                })
            };
            // Durable dead-credential verdict, consulted when no live store has
            // an answer. It outranks the mtime derivation because that
            // derivation can only ever say Fresh/Cached: without this, a warm
            // cache behind a session that will NEVER self-heal published
            // "Fresh" — a live measurement over a dead credential, on every
            // daemonless surface. Bound to the credential that produced it, so
            // a re-login retires it and a profile nothing ever fetched has none.
            let recorded_expired = || {
                crate::usage::profile_credential_fingerprint(p)
                    .is_some_and(|fp| crate::profile_cache::auth_expired_matches(name, fp))
                    .then_some(fetch_status_str(FetchStatus::AuthExpired))
            };
            let fetch_status: Option<&'static str> = match live {
                Some(sig) => sig
                    .status
                    .get(name.as_str())
                    .or_else(|| sig.third_party_status.get(name.as_str()))
                    .copied()
                    .map(fetch_status_str)
                    .or_else(recorded_expired)
                    .or_else(derived_status),
                None => recorded_expired().or_else(derived_status),
            };

            // next_refresh_at: the live countdown store, else mtime + interval
            // (also the fallback for names the live store doesn't carry). A
            // spent OAuth account under `refresh_spent_accounts` OFF has no
            // pending refresh — the scheduler blanks its live entry, so guard the
            // derivation too, else it falls through to a past mtime+interval
            // stamp that reads as perpetually overdue.
            //
            // Excluded on the cache selector, not `is_third_party`: the skip
            // this mirrors (`drop_spent_oauth`) blanks the OAUTH leg's map
            // alone, so an account the third-party leg also fetches keeps that
            // leg's countdown — a hybrid is spent on one leg and pending on the
            // other. That predicate also pins the constant below: the `&&`
            // reaches it only where `usage_cache_file` resolves to that file.
            let derived_next = || mtime_ms.map(|mt| mt.saturating_add(interval_ms));
            let spent_skipped = !config.state.refresh_spent_accounts
                && !p.usage_cache_is_third_party()
                && load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
                    .is_some_and(|u| windows_maxed(&u, (now / 1000) as i64));
            let next_refresh_ms: Option<u64> = if spent_skipped {
                None
            } else {
                match live {
                    Some(sig) => sig
                        .next_refresh
                        .get(name.as_str())
                        .copied()
                        .or_else(derived_next),
                    None => derived_next(),
                }
            };

            // `stale` = the daemon distrusts this reading — a deep-slot stuck
            // RateLimited (live status RateLimited AND the 429 streak past the
            // active cap). Read from the OAuth `status` store ALONE, deliberately
            // narrower than the `fetch_status` above: the streak counter it pairs
            // with is written only by `apply_outcome`, the OAuth leg's own
            // handler, so a third-party 429 has no streak to judge and would
            // always read as a shallow one. Never true for the single-shot (no
            // streaks). Same predicate `scan_auto_switch` distrusts, so the
            // published flag and the switch decision cannot drift.
            let stale = match live {
                Some(sig) => sig.status.get(name.as_str()).copied().is_some_and(|s| {
                    is_stuck_rate_limited(s, sig.streaks.get(name.as_str()).copied().unwrap_or(0))
                }),
                None => false,
            };

            // Structured third-party balance isn't carried by ThirdPartyStats
            // (it lives in free-text `rows`); expose only the availability flag
            // for now — enough for a reader's red/green reachability dot.
            //
            // Same question the freshness above asks — where do this account's
            // figures live — so it takes the same predicate. Keyed on
            // `is_third_party` it published a null dot for every generic api-key
            // endpoint while `fetched_at` beside it dated that account's provider
            // cache: one object, two answers about the same file.
            let third_party = if p.usage_cache_is_third_party() {
                load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                    .map(|s| serde_json::json!({ "available": s.is_available }))
            } else {
                None
            };

            ProfileEntry {
                name: name.clone(),
                active: config.is_active(name),
                rolling_token: matches!(
                    crate::claude::sidecar_summary(name),
                    Some((crate::claude::SidecarKind::Rolling, _))
                ),
                provider: provider_label(p),
                base_url: p.base_url.clone(),
                tier: tier_label(p),
                has_live_session: crate::runtime::has_live_session(name),
                auth_status: auth_status_str(config, p, now as i64).to_string(),
                fetch_status: fetch_status.map(str::to_string),
                stale,
                fetched_at: mtime_ms.map(iso_from_ms),
                next_refresh_at: next_refresh_ms.map(iso_from_ms),
                auto_start: p.auto_start,
                auto_start_queue: queue_members
                    .iter()
                    .position(|n| n.as_str() == name.as_str())
                    .map(|i| QueueEntry {
                        position: i + 1,
                        next_open_at: next_queue_open.map(iso_from_ms),
                    }),
                bell_threshold: p.bell_threshold,
                fallback: fallback_json(config, p),
                windows: published_windows(name),
                third_party,
            }
        })
        .collect()
}

/// Build the full `status.json` body. `interval_ms` is the live refresh interval
/// (daemon) or `config.state.refresh_interval_ms` (single-shot). `live` carries
/// the scheduler's in-memory freshness/countdown stores when a daemon is running.
pub(crate) fn build_status(
    config: &AppConfig,
    interval_ms: u64,
    live: Option<&LiveSignals>,
    include_disabled: bool,
) -> serde_json::Value {
    let profiles = build_profile_entries(config, interval_ms, live, include_disabled);
    // Stamped after the entries build (each entry reads its own clock) so
    // `generated_at` never precedes the instant a per-entry verdict was judged at.
    let now = now_ms();

    serde_json::json!({
        "schema": SCHEMA_VERSION,
        "generated_at": iso_from_ms(now),
        "active_profile": config.state.active_profile.as_deref(),
        "pending_switch": live.and_then(|s| s.pending_switch),
        "wrap_off": config.state.switch_off_when_spent,
        "refresh_interval_ms": interval_ms,
        "profiles": profiles,
    })
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_status_json.rs"]
mod tests;
