//! `~/.clauth/status.json` serializer — the daemon's published feed, and the
//! shape `clauth status --json` prints (one code path builds both, so they
//! cannot drift). Contract: wiki/Daemon.md.
//!
//! Usage windows/tier come from the on-disk usage caches — `usage_cache.json`
//! for an OAuth account, `third_party_cache.json` for an api-key one — written
//! by the scheduler, so this is process-independent: it returns the
//! last-persisted numbers whether or not a scheduler is live. Two fields —
//! `fetch_status` and
//! `next_refresh_at` — live only in the scheduler's in-memory stores; when a
//! live daemon passes [`LiveSignals`] they come from there, otherwise they are
//! derived from the cache-file mtime so the single-shot `status --json` still
//! produces a coherent shape.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::gateway::{GatewaySlot, slot_or_record};
use super::proxies::ProxySlot;
use crate::profile::{AppConfig, Profile, ProfileName};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::profile_json::{
    OauthAge, Window, oauth_age, provider_label, published_windows, publishes_a_live_window,
    stale_after_ms, tier_label, usage_cache_file,
};
use crate::providers::ThirdPartyStats;
use crate::usage::{
    FetchStatus, LegKey, UsageInfo, epoch_secs_to_iso, is_stuck_rate_limited, now_ms,
    selected_next_refresh, windows_maxed,
};

/// Bump when the JSON shape changes in a way readers must branch on. 2: the
/// `auth_status` value `expiring` was renamed to `expired` (breaking — a
/// reader keying on the old word must refuse or translate).
pub(crate) const SCHEMA_VERSION: u64 = 2;

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
    /// into `status`: `stale`'s stuck arm is contracted as a stuck 429 read off
    /// the OAuth store, and folding the two would silently retarget it.
    pub(crate) third_party_status: &'a HashMap<String, FetchStatus>,
    /// The third-party consecutive-429 streak, so `stale`'s stuck arm can judge a
    /// member with no OAuth status entry off the same predicate the switch
    /// decision uses (OAuth-first, third-party fallback).
    pub(crate) third_party_streaks: &'a HashMap<String, u32>,
    pub(crate) next_refresh: &'a HashMap<LegKey, u64>,
    /// Consecutive-429 streaks, so a profile whose live `status` is `RateLimited`
    /// AND whose streak has passed the active cap can be published as `stale` (a
    /// deep-slot stuck read the daemon distrusts — the same judgment
    /// `scan_auto_switch` acts on). Empty for the single-shot `status --json` (no
    /// daemon), so the STUCK arm is always `false` there; the age arm needs no
    /// store and fires on both paths.
    pub(crate) streaks: &'a HashMap<String, u32>,
    /// The switch target the daemon has accepted but not yet applied (from
    /// `pending_switch`), so a reader can show in-flight truth instead of a
    /// timing heuristic. `None` for the single-shot `status --json` (no daemon).
    pub(crate) pending_switch: Option<&'a str>,
    /// The scheduler's in-memory auto-start queue anchor
    /// ([`crate::usage::queue_anchor_cached`]), so the published `next_open_at`
    /// carries the anchor the scheduler holds. The gate composes this CACHED
    /// value by `max` with a per-tick history derivation, so the published
    /// stamp can only be EARLIER than the anchor a tick will gate on — a
    /// reader acting on it is early at worst, never late. `None` while
    /// nothing has opened — published as a `null` `next_open_at`, which reads
    /// as "due now". The single-shot `status --json` has no scheduler and
    /// derives it from the usage-history series instead
    /// ([`crate::usage::history_anchor`]) — one replay per invocation, a cost
    /// the per-tick daemon feed must not pay.
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
    /// The gateway slot the daemon's supervisor last published. `None`
    /// single-shot or before the supervisor's first publish, where the slot
    /// reads the gateway record alone ([`super::gateway::unsupervised_slot`]).
    pub(crate) gateway: Option<&'a GatewaySlot>,
    /// The per-proxy slots the daemon's supervisors last published, in
    /// service-name order. `None` single-shot or before the supervisors'
    /// first publish, where each slot reads its row alone
    /// ([`super::proxies::unsupervised_slot`]).
    pub(crate) proxies: Option<&'a [ProxySlot]>,
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

/// The `fallback` object for a profile: chain membership (`position` is
/// 1-based), the utilization threshold auto-switch rotates away at, and whether
/// this member is currently armed (`armed` = in the chain AND active). Field
/// order is the published key order.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct Fallback {
    pub(crate) position: usize,
    pub(crate) threshold: f64,
    pub(crate) armed: bool,
}

/// The chain-membership object for a profile, or `None` when it is not a chain
/// member.
fn fallback(config: &AppConfig, p: &Profile) -> Option<Fallback> {
    let name = &p.name;
    let pos = config.state.fallback_chain.iter().position(|n| n == name)?;
    Some(Fallback {
        position: pos + 1,
        threshold: crate::fallback::threshold_for(p),
        armed: config.is_active(name),
    })
}

/// Per-profile auth health for `status.json`. `broken` (last refresh rejected
/// as revoked/invalid — `AppState::auth_broken`) outranks `expired` (an OAuth
/// access token past its expiry, refresh not yet run); everything else is
/// `ok`. Readers default an absent field to `ok` (the additive-evolution
/// rule); it is still emitted for an explicit, greppable contract.
///
/// Keyed on credential typing ([`Profile::login_is_oauth`]), not endpoint routing:
/// this reports on the token the profile STORES, and a hybrid (an OAuth pair plus
/// a `base_url`) holds one that expires like any other. Reading it behind the
/// endpoint gate published a permanent `ok` over a dead token.
fn auth_status_str(config: &AppConfig, p: &Profile, now_ms: i64) -> &'static str {
    if config.is_auth_broken(&p.name) {
        return "broken";
    }
    if p.login_is_oauth() && p.access_token_expires_at().is_some_and(|exp| now_ms >= exp) {
        return "expired";
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct QueueEntry {
    pub(crate) position: usize,
    #[schema(required = true)]
    pub(crate) next_open_at: Option<String>,
}

/// The third-party availability object (`available`), for api-key accounts
/// whose figures live in `third_party_cache.json`; `None` for OAuth accounts.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct ThirdPartyAvailability {
    pub(crate) available: bool,
}

/// One `profiles[]` entry of the published `status.json` body — the shape both
/// the writer ([`build_profile_entries`], serialized by [`build_status`]) and
/// the reader (`clauth list`'s table rows) derive from, so a reader's field
/// access cannot drift from what the writer emits. Contract: wiki/Daemon.md.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct ProfileEntry {
    #[schema(value_type = String)]
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
    #[schema(required = true)]
    pub(crate) base_url: Option<String>,
    /// Human tier label for an anthropic account (`Max 5x`); `None` for
    /// third-party/api-key profiles.
    #[schema(required = true)]
    pub(crate) tier: Option<String>,
    /// Additive (schema stays 1): which harness this profile belongs to,
    /// `claude` or `codex`. Membership of a state file is the authority
    /// (decision 1) and names are globally unique (decision 2), so one flat
    /// `profiles[]` still reads unambiguously — a reader that predates codex
    /// ignores the field and sees the claude accounts it always saw, because
    /// codex entries are appended after them.
    pub(crate) harness: String,
    /// A live `clauth start` session runs for this profile.
    pub(crate) has_live_session: bool,
    /// `ok` / `expired` / `broken` (see [`auth_status_str`]).
    pub(crate) auth_status: String,
    /// Freshness: a live daemon's verdict or the cache-mtime derivation; `None`
    /// when there is no cache at all.
    #[schema(required = true)]
    pub(crate) fetch_status: Option<String>,
    /// Additive: true when this reading is distrusted, by
    /// either arm — a deep-slot stuck RateLimited, or reading age past
    /// `stale_after_ms(interval)` (the stuck arm needs the live stores and is
    /// `false` single-shot). Readers dim it / show a "stuck" cue instead of
    /// treating it as current truth.
    pub(crate) stale: bool,
    /// ISO-8601 UTC stamp of when the published figures were last fetched
    /// (OAuth: the body's `fetched_at`; third-party: the cache write);
    /// `None` when there is no cache or the body is undated.
    #[schema(required = true)]
    pub(crate) fetched_at: Option<String>,
    /// ISO-8601 UTC stamp of the next scheduled refresh; `None` when none is
    /// pending (a spent skipped account, or no cache).
    #[schema(required = true)]
    pub(crate) next_refresh_at: Option<String>,
    pub(crate) auto_start: bool,
    /// Additive: this profile's slot in the interleaved
    /// auto-start queue, `None`/`null` when it holds none — the toggle is off,
    /// it never opted into `auto_start`, or it cannot open a window.
    /// `default` so a reader stays additive-tolerant of an older writer.
    #[serde(default)]
    #[schema(required = true)]
    pub(crate) auto_start_queue: Option<QueueEntry>,
    #[schema(required = true)]
    pub(crate) bell_threshold: Option<f64>,
    /// The chain-membership object (`position` / `threshold` / `armed`), `None`
    /// when not a chain member.
    #[schema(required = true)]
    pub(crate) fallback: Option<Fallback>,
    /// The 5h/7d usage rows: an OAuth account's own windows, or an api-key
    /// account's provider-derived ones. Empty when the cache behind them
    /// holds none.
    pub(crate) windows: Vec<Window>,
    /// The third-party availability object (`available`), `None` for OAuth
    /// accounts.
    #[schema(required = true)]
    pub(crate) third_party: Option<ThirdPartyAvailability>,
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

            // The OAuth disk body, loaded once and shared by the spent-skip
            // exemption, the freshness derivations and the age arm below —
            // all read the DISK cache, never the live store (a spent account
            // the scheduler dropped keeps its store entry, so the two can
            // disagree exactly on the exempted state).
            let oauth_usage = if p.usage_cache_is_third_party() {
                None
            } else {
                load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
            };

            // The clock both mtime derivations below read. A DATED OAuth body
            // dates off its own `fetched_at` stamp — the same age contract
            // (`oauth_age`) every other surface reads — because a plan-only
            // cache rewrite (`apply_outcome`'s `plan_refresh` write, the
            // hourly `/profile` ride on a 429'd `/usage`) moves the mtime
            // without producing a new reading, and dating off it let that
            // rewrite re-age the account (R8, #74). An UNDATABLE body (a
            // plan-only cold fill, a pre-`fetched_at` cache) has no stamp to
            // trust, so the mtime is the only clock left (known-movable; the
            // account it describes carries no fetch to date), as does a
            // third-party cache: that file's only writer is a fetch outcome.
            let derived_clock_ms = if p.usage_cache_is_third_party() {
                mtime_ms
            } else {
                match oauth_age(oauth_usage.as_ref(), now) {
                    OauthAge::Dated(_) => oauth_usage.as_ref().and_then(|u| u.fetched_at),
                    OauthAge::Absent | OauthAge::Undated => mtime_ms,
                }
            };

            // fetch_status: the live stores when a daemon is running, else
            // derive from the last real fetch's recency (Fresh within one
            // interval, else Cached) off `derived_clock_ms`. A name in NEITHER
            // live store (a just-started daemon, the single-shot
            // `status --json`) falls back to that derivation rather than
            // reading as never-fetched; null = no cache at all.
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
                derived_clock_ms.map(|at| {
                    if now.saturating_sub(at) < interval_ms {
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

            // next_refresh_at: the live countdown store, else the derived
            // clock + interval (also the fallback for names the live store
            // doesn't carry). A derived stamp already past (`now >= clock +
            // interval`) publishes None — the single-shot has no live
            // countdown to vouch for it, so an overdue stamp would read as
            // perpetually overdue (#74). Live-store stamps stay verbatim: a
            // daemon's own countdown is real.
            // A spent OAuth account under `refresh_spent_accounts` OFF has no
            // pending refresh — the scheduler blanks its live entry, so
            // `spent_skipped` guards the derivation too.
            //
            // Excluded on the cache selector, not `is_third_party`: the skip
            // this mirrors (`drop_spent_oauth`) blanks the OAUTH leg's map
            // alone, so an account the third-party leg also fetches keeps that
            // leg's countdown — a hybrid is spent on one leg and pending on the
            // other. That predicate also pins the constant below: the `&&`
            // reaches it only where `usage_cache_file` resolves to that file.
            let derived_next = || {
                derived_clock_ms.and_then(|at| {
                    let stamp = at.saturating_add(interval_ms);
                    (stamp > now).then_some(stamp)
                })
            };
            let spent_skipped = !config.state.refresh_spent_accounts
                && oauth_usage
                    .as_ref()
                    .is_some_and(|u| windows_maxed(u, (now / 1000) as i64));
            let next_refresh_ms: Option<u64> = if spent_skipped {
                None
            } else {
                match live {
                    Some(sig) => selected_next_refresh(sig.next_refresh, p).or_else(derived_next),
                    None => derived_next(),
                }
            };

            // `stale` = the daemon distrusts this reading, by either arm:
            //
            // * a deep-slot stuck RateLimited (live status RateLimited AND the
            //   429 streak past the active cap). OAuth-first with a third-party
            //   fallback: a member with no OAuth status entry is judged off its
            //   third-party status + streak — the same predicate
            //   `scan_auto_switch` distrusts — so the published flag and the
            //   switch decision cannot drift for a third-party member either.
            // * cache AGE past `2 × max(interval, 5min) + interval` (#74): a
            //   figure that old is one nothing is maintaining, live scheduler
            //   or none. Keyed to the LIVE interval (a long interval is an
            //   operator's own chosen cadence, so the threshold scales with
            //   it), floored at 5min so a tight cadence never shortens the
            //   grace below what a degraded fetch can legally leave. The
            //   live-maxed exemption below is inherited via `spent_skipped`:
            //   a window pinned at the API's 100% cap cannot change by
            //   polling, so age distrusts nothing about it.
            // OAuth AGE goes through the one contract (`oauth_age`), so this
            // feed's `stale`, the TUI cue and the MCP payloads cannot answer
            // differently about the same file. `fetch_status` and
            // `next_refresh_at` above are a separate question (the last fetch
            // OUTCOME, not the reading's age) and read `derived_clock_ms`. The
            // third-party leg dates off that mtime too, its only writer being
            // a fetch outcome. An OAuth body with no stamp or a future one is
            // stale with no age published: its figures stay visible, and
            // nothing claims to date them.
            let (age_source_ms, past_threshold) = if p.usage_cache_is_third_party() {
                (
                    mtime_ms,
                    mtime_ms.is_some_and(|at| now.saturating_sub(at) > stale_after_ms(interval_ms)),
                )
            } else {
                let age = oauth_age(oauth_usage.as_ref(), now);
                // The stamp publishes only when this feed trusts it: an undated
                // or future-stamped body carries `stale` with no `fetched_at`.
                let at = match age {
                    OauthAge::Dated(_) => oauth_usage.as_ref().and_then(|u| u.fetched_at),
                    OauthAge::Absent | OauthAge::Undated => None,
                };
                (
                    at,
                    age.is_stale(
                        stale_after_ms(interval_ms),
                        oauth_usage.as_ref().is_some_and(publishes_a_live_window),
                    ),
                )
            };
            let age_stale = !spent_skipped && past_threshold;
            let stale = match live {
                Some(sig) => match sig.status.get(name.as_str()).copied() {
                    Some(s) => is_stuck_rate_limited(
                        s,
                        sig.streaks.get(name.as_str()).copied().unwrap_or(0),
                    ),
                    None => sig
                        .third_party_status
                        .get(name.as_str())
                        .copied()
                        .is_some_and(|s| {
                            is_stuck_rate_limited(
                                s,
                                sig.third_party_streaks
                                    .get(name.as_str())
                                    .copied()
                                    .unwrap_or(0),
                            )
                        }),
                },
                None => false,
            } || age_stale;

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
                load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE).map(|s| {
                    ThirdPartyAvailability {
                        available: s.is_available,
                    }
                })
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
                harness: "claude".to_string(),
                has_live_session: crate::runtime::has_live_session(name),
                auth_status: auth_status_str(config, p, now as i64).to_string(),
                fetch_status: fetch_status.map(str::to_string),
                stale,
                fetched_at: age_source_ms.map(iso_from_ms),
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
                fallback: fallback(config, p),
                windows: published_windows(name),
                third_party,
            }
        })
        .collect()
}

/// The codex half of `profiles[]`, appended after the claude entries.
///
/// Built from the caller's `codex-profiles.toml` read (one load per body, so
/// these `active` flags and the top-level `active_codex_profile` can never
/// disagree) plus the per-profile usage cache the codex leg writes, and
/// nothing else: a codex profile has no `Profile` record (the file split
/// leaves `profiles.toml` untouched), so every claude-only field is its
/// no-data form rather than a fabricated one. `tier` carries the ChatGPT plan
/// — the polled one, else the id_token's claim — never a `Claude <tier>`
/// label, which is what `tier_label` would produce.
pub(crate) fn build_codex_entries(
    codex: &crate::codex_profiles::CodexState,
    interval_ms: u64,
) -> Vec<ProfileEntry> {
    let now = now_ms();
    let active = codex.active_profile();
    codex
        .profiles()
        .iter()
        .map(|name| {
            let mtime_ms = profile_cache_mtime_ms(name, USAGE_CACHE_FILE);
            let cached: Option<UsageInfo> = load_profile_cache(name, USAGE_CACHE_FILE);
            ProfileEntry {
                name: name.clone(),
                active: active.is_some_and(|a| a == name),
                // Rolling tokens are a claude-side mechanism (a `session-token`
                // sidecar); codex holds one chain in one auth.json.
                rolling_token: false,
                provider: "openai".to_string(),
                base_url: None,
                tier: crate::codex_auth::plan_label(
                    name.as_str(),
                    cached
                        .as_ref()
                        .and_then(|u| u.plan.as_ref())
                        .and_then(|p| p.codex_plan.as_deref()),
                ),
                harness: "codex".to_string(),
                has_live_session: crate::runtime::has_live_session(name),
                // `broken` is the server's terminal verdict on the chain
                // (`codex_auth::read_quarantine`), the codex twin of the claude
                // `auth_broken` grade; `expired` has no codex reading, since the
                // standby leg rotates on the access token's own clock.
                auth_status: if crate::codex_auth::read_quarantine(name.as_str()).is_some() {
                    "broken"
                } else if cached.is_some() {
                    "ok"
                } else {
                    "unknown"
                }
                .to_string(),
                fetch_status: mtime_ms.map(|mt| {
                    if now.saturating_sub(mt) < interval_ms {
                        "Fresh"
                    } else {
                        "Cached"
                    }
                    .to_string()
                }),
                stale: false,
                fetched_at: mtime_ms.map(iso_from_ms),
                next_refresh_at: mtime_ms.map(|mt| iso_from_ms(mt.saturating_add(interval_ms))),
                auto_start: false,
                // The interleaved auto-start queue elects claude members only.
                auto_start_queue: None,
                bell_threshold: None,
                fallback: None,
                windows: published_windows(name),
                third_party: None,
            }
        })
        .collect()
}

/// The full `status.json` body. Field order is the published key order, and
/// each `Option` field emits a present key holding `null` when absent.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct StatusBody {
    pub(crate) schema: u64,
    pub(crate) generated_at: String,
    #[schema(required = true)]
    pub(crate) active_profile: Option<String>,
    #[schema(required = true)]
    pub(crate) pending_switch: Option<String>,
    pub(crate) wrap_off: bool,
    /// Additive per-harness slots (decision 10): the top-level
    /// `active_profile` / `wrap_off` above stay the CLAUDE ones, so nothing
    /// that reads them today changes meaning. `default` so a reader stays
    /// additive-tolerant of an older writer.
    #[serde(default)]
    #[schema(required = true)]
    pub(crate) active_codex_profile: Option<String>,
    #[serde(default)]
    #[schema(value_type = Vec<String>)]
    pub(crate) codex_fallback_chain: Vec<ProfileName>,
    #[serde(default)]
    pub(crate) codex_wrap_off: bool,
    pub(crate) refresh_interval_ms: u64,
    /// The daemon that wrote this feed. A reader can tell an old daemon —
    /// one with no codex support at all — from a new one reporting an empty
    /// codex roster, which are otherwise byte-identical.
    #[serde(default)]
    pub(crate) clauth_version: String,
    /// Additive: the managed shunt gateway, the object `GET /api/v1/gateway`
    /// serves. `default` so a reader stays additive-tolerant of an older
    /// writer.
    #[serde(default)]
    #[schema(required = true)]
    pub(crate) gateway: Option<GatewaySlot>,
    /// Additive: the managed proxies, one object per registry row, the array
    /// `GET /api/v1/proxies` serves. `default` so a reader stays
    /// additive-tolerant of an older writer.
    #[serde(default)]
    #[schema(required = true)]
    pub(crate) proxies: Vec<ProxySlot>,
    pub(crate) profiles: Vec<ProfileEntry>,
}

/// Build the full `status.json` body. `interval_ms` is the live refresh interval
/// (daemon) or `config.state.refresh_interval_ms` (single-shot). `live` carries
/// the scheduler's in-memory freshness/countdown stores when a daemon is running.
pub(crate) fn build_status(
    config: &AppConfig,
    interval_ms: u64,
    live: Option<&LiveSignals>,
    include_disabled: bool,
) -> StatusBody {
    let mut profiles = build_profile_entries(config, interval_ms, live, include_disabled);
    // One read feeds both the entries and the slots below, so a load error
    // publishes an empty roster AND empty slots rather than a body whose two
    // halves describe different files.
    let codex = crate::codex_profiles::CodexState::load().unwrap_or_default();
    // Appended, never interleaved: a reader that predates codex takes the
    // prefix it always took.
    profiles.extend(build_codex_entries(&codex, interval_ms));
    // Stamped after the entries build (each entry reads its own clock) so
    // `generated_at` never precedes the instant a per-entry verdict was judged at.
    let now = now_ms();

    StatusBody {
        schema: SCHEMA_VERSION,
        generated_at: iso_from_ms(now),
        active_profile: config.state.active_profile.as_deref().map(str::to_string),
        pending_switch: live.and_then(|s| s.pending_switch).map(str::to_string),
        wrap_off: config.state.switch_off_when_spent,
        active_codex_profile: codex.active_profile().map(|n| n.as_str().to_string()),
        codex_fallback_chain: codex.fallback_chain().to_vec(),
        codex_wrap_off: codex.switch_off_when_spent(),
        refresh_interval_ms: interval_ms,
        clauth_version: env!("CARGO_PKG_VERSION").to_string(),
        gateway: Some(slot_or_record(live.and_then(|s| s.gateway))),
        proxies: super::proxies::entries(live.and_then(|s| s.proxies)),
        profiles,
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_status_json.rs"]
mod tests;
