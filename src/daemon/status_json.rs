//! `~/.clauth/status.json` serializer — the canonical read format for the
//! menu-bar app (`ccsbar`) and `clauth status --json`.
//!
//! Usage windows/tier come from the on-disk `usage_cache.json` (written by the
//! scheduler), so this is process-independent: it returns the last-persisted
//! numbers whether or not a scheduler is live. Two fields — `fetch_status` and
//! `next_refresh_at` — live only in the scheduler's in-memory stores; when a
//! live daemon passes [`LiveSignals`] they come from there, otherwise they are
//! derived from the cache-file mtime so the single-shot `status --json` still
//! produces a coherent shape.

use std::collections::HashMap;

use crate::profile::{AppConfig, Profile};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::profile_json::{provider_label, tier_label, windows_json};
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
    /// TECH-6: the last drain skip/failure reason `(at_ms, message)`, so a tap
    /// that can't land immediately is observable. `None` for the single-shot
    /// `status --json` (no daemon has drained anything) and until the first skip.
    pub(crate) last_error: Option<(u64, &'a str)>,
    /// TECH-8: the last executed switch (the hero event). `None` for the single-shot
    /// `status --json` and until the daemon executes its first switch.
    pub(crate) last_switch: Option<&'a super::LastSwitch>,
}

fn fetch_status_str(s: FetchStatus) -> &'static str {
    match s {
        FetchStatus::Fresh => "Fresh",
        FetchStatus::Cached => "Cached",
        FetchStatus::Failed => "Failed",
        FetchStatus::RateLimited => "RateLimited",
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
    let name = p.name.as_str();
    // CDX-4 C4: a profile's fallback block reads against ITS harness's chain
    // — position within that chain, armed against that harness's active slot.
    let (chain, armed) = if p.is_codex() {
        (
            &config.state.codex_fallback_chain,
            config.is_active_codex(name),
        )
    } else {
        (&config.state.fallback_chain, config.is_active(name))
    };
    let pos = chain.iter().position(|n| n.as_str() == name)?;
    Some(serde_json::json!({
        "position": pos + 1,
        "threshold": crate::fallback::threshold_for(p),
        "armed": armed,
        // Additive (schema stays 1): the exclusive last-resort mark — this member
        // is accepted by the walk's sink pass even while exhausted.
        "last_resort": p.last_resort,
        // Additive (SCW-2/WKO): the per-account usage gates and weekly-line
        // override, so a client can render/edit the same per-member controls
        // the Fallback tab carries (`set_check_weekly` / `set_check_scoped` /
        // `set_member_weekly` on the socket). `weekly_threshold` is null when
        // the member follows the chain-wide line.
        "check_weekly": p.check_weekly,
        "check_scoped": p.check_scoped,
        "weekly_threshold": p.weekly_threshold,
    }))
}

/// The daemon's OWN next-move forecast, computed by the same
/// [`crate::fallback::next_target`] walk the switch decision runs — so the
/// menu-bar app renders the daemon's own walk instead of mirroring the
/// walk logic client-side. Additive (schema stays 1). Drift-proof by
/// construction: when upstream changed the walk semantics (burn-aware active
/// check, exclusive `last_resort` sinks — 2026-07), a client-side mirror
/// silently lagged; a published forecast cannot.
///
/// `action` is `"switch"` (with `to`), `"off"` (wrap-off would halt every
/// account), or `"none"` (nothing viable / no chain). The burn rate feeding the
/// wrap-off decision is sourced exactly like the scheduler sources it — the
/// cached 5h window plus on-disk history — and only when burn-aware switching
/// is on (it is unused otherwise, so the extra reads are skipped).
///
/// This is a PROJECTION of the walk's pick — "where the chain would go" — not
/// a statement that a switch is due this tick: the live decision
/// (`scan_auto_switch`) additionally gates on the active's fetch freshness and
/// exhaustion (or its auth-broken flag, AUTH-4) before acting. A healthy
/// active with a viable sibling therefore publishes `switch` here while the
/// scheduler correctly stays put; readers must render it as "would switch to
/// X", never as "switching now".
fn forecast_json(config: &AppConfig) -> serde_json::Value {
    // `next_target` judges members through `Profile.usage`, which only the TUI
    // thread populates (`apply_usage`) — in the daemon it is None for every
    // profile, so the un-hydrated walk saw universal headroom and forecast the
    // first non-active member even when its week was spent (observed
    // 2026-07-08: the forecast pointed at a 7d=100 account the real switch
    // decision correctly refuses). Hydrate a snapshot from the same
    // per-profile disk caches the scheduler persists on every fetch — the
    // exact source this status.json's own `windows` field is built from, so
    // the forecast and the numbers beside it cannot disagree.
    let hydrated = AppConfig {
        state: config.state.clone(),
        profiles: config
            .profiles
            .iter()
            .map(|p| {
                let mut p = p.clone();
                if p.usage.is_none() {
                    p.usage = load_profile_cache::<UsageInfo>(p.name.as_str(), USAGE_CACHE_FILE);
                }
                p
            })
            .collect(),
    };
    let config = &hydrated;
    let rate = config
        .state
        .burn_aware_switching
        .then(|| {
            let active = config.state.active_profile.as_deref()?;
            let window = load_profile_cache::<UsageInfo>(active, USAGE_CACHE_FILE)?.five_hour?;
            crate::fallback::burn_rate_for_profile(active, &window)
        })
        .flatten();
    match crate::fallback::next_target(config, rate) {
        Some(crate::fallback::SwitchAction::To(name)) => {
            serde_json::json!({ "action": "switch", "to": name })
        }
        Some(crate::fallback::SwitchAction::Off) => {
            serde_json::json!({ "action": "off", "to": null })
        }
        None => serde_json::json!({ "action": "none", "to": null }),
    }
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
    if config.is_auth_broken(p.name.as_str()) {
        return "broken";
    }
    if p.login_is_oauth() && p.access_token_expires_at().is_some_and(|exp| now_ms >= exp) {
        return "expiring";
    }
    "ok"
}

/// Build the full `status.json` body. `interval_ms` is the live refresh interval
/// (daemon) or `config.state.refresh_interval_ms` (single-shot). `live` carries
/// the scheduler's in-memory freshness/countdown stores when a daemon is running.
/// `include_disabled` gates whether a user-disabled account appears in the
/// `profiles` array at all — the daemon's own `status.json` feed always passes
/// `false` (hidden by default); the single-shot `clauth status --json --all`/
/// `--disabled` flag flips it to `true`. The active profile is ALWAYS kept, disabled or not: the
/// top-level `active_profile` field names it unconditionally, and a reader
/// following this contract resolves that name against `profiles[]` — hiding it
/// there would leave `active_profile` dangling.
pub(crate) fn build_status(
    config: &AppConfig,
    interval_ms: u64,
    live: Option<&LiveSignals>,
    include_disabled: bool,
) -> serde_json::Value {
    let now = now_ms();
    let profiles: Vec<serde_json::Value> = config
        .profiles
        .iter()
        .filter(|p| include_disabled || !p.is_disabled() || config.is_active(p.name.as_str()))
        .map(|p| {
            let name = p.name.as_str();
            // Freshness reads each profile's OWN cache: the third-party leg
            // never touches USAGE_CACHE_FILE, so keying api-key profiles on it
            // rendered a healthy hourly-refreshed account as never-fetched.
            let cache_file = if p.is_third_party() {
                THIRD_PARTY_CACHE_FILE
            } else {
                USAGE_CACHE_FILE
            };
            let mtime_ms = profile_cache_mtime_ms(name, cache_file);

            // fetch_status: the live store when a daemon is running, else derive
            // from cache freshness (Fresh within one interval, else Cached; a
            // profile with no cache at all reports null — never fetched). The
            // live store only carries the OAuth leg's outcomes, so a name
            // missing from it (api-key profiles, a just-started daemon) falls
            // back to the same mtime derivation instead of reading null.
            let derived_status = || {
                mtime_ms.map(|mt| {
                    if now.saturating_sub(mt) < interval_ms {
                        "Fresh"
                    } else {
                        "Cached"
                    }
                })
            };
            let fetch_status: Option<&'static str> = match live {
                Some(sig) => sig
                    .status
                    .get(name)
                    .copied()
                    .map(fetch_status_str)
                    .or_else(derived_status),
                None => derived_status(),
            };

            // next_refresh_at: the live countdown store, else mtime + interval
            // (also the fallback for names the live store doesn't carry). A
            // spent OAuth account under `refresh_spent_accounts` OFF has no
            // pending refresh — the scheduler blanks its live entry, so guard the
            // derivation too, else it falls through to a past mtime+interval
            // stamp that reads as perpetually overdue.
            let derived_next = || mtime_ms.map(|mt| mt.saturating_add(interval_ms));
            let spent_skipped = !config.state.refresh_spent_accounts
                && !p.is_third_party()
                && load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
                    .is_some_and(|u| windows_maxed(&u, (now / 1000) as i64));
            let next_refresh_ms: Option<u64> = if spent_skipped {
                None
            } else {
                match live {
                    Some(sig) => sig.next_refresh.get(name).copied().or_else(derived_next),
                    None => derived_next(),
                }
            };

            // `stale` = the daemon distrusts this reading — a deep-slot stuck
            // RateLimited (live status RateLimited AND the 429 streak past the
            // active cap). Read from the LIVE store, not the mtime-derived
            // fetch_status string (which never yields RateLimited), so it is only
            // ever true under a real daemon. The single-shot has no streaks →
            // always false. Same predicate `scan_auto_switch` distrusts, so the
            // published flag and the switch decision cannot drift.
            let stale = match live {
                Some(sig) => sig.status.get(name).copied().is_some_and(|s| {
                    is_stuck_rate_limited(s, sig.streaks.get(name).copied().unwrap_or(0))
                }),
                None => false,
            };

            // Structured third-party balance isn't carried by ThirdPartyStats
            // (it lives in free-text `rows`); expose only the availability flag
            // for now — the menu bar shows a red/green dot. Balance deferred.
            let third_party = if p.is_third_party() {
                load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                    .map(|s| serde_json::json!({ "available": s.is_available }))
            } else {
                None
            };

            // CDX-1 T7: parse the stored codex snapshot once — identity, plan
            // and expiry all live in its JWTs (zero network). None for claude
            // profiles and for a codex profile with no captured login yet.
            let codex_auth = p
                .is_codex()
                .then(|| crate::codex::read_profile_auth(name).ok().flatten())
                .flatten()
                .and_then(|bytes| crate::codex::CodexAuthFile::parse(&bytes).ok());
            // Pinned ccsbar contract (docs/ccsbar/DESIGN.md): when the stored
            // snapshot was last captured/adopted — file mtime, codex-only.
            let codex_snapshot_ms: Option<u64> = p
                .is_codex()
                .then(|| {
                    let path = crate::codex::profile_auth_path(name).ok()?;
                    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
                    let ms = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
                    Some(ms.as_millis() as u64)
                })
                .flatten();

            serde_json::json!({
                "name": name,
                // One coherent boolean per profile: a codex profile reports the
                // codex-slot truth, a claude profile the claude-slot truth. The
                // two slots are independent (docs/codex-support/PLAN.md §0.1).
                "active": if p.is_codex() {
                    config.is_active_codex(name)
                } else {
                    config.is_active(name)
                },
                // Additive (schema stays 1): which CLI this profile's
                // credentials belong to. Absent in pre-CDX writers — readers
                // default to "claude".
                "harness": if p.is_codex() { "codex" } else { "claude" },
                // Additive (CLA-FEED): the session-token feed — the sidecar's
                // hours-scale countdown is routine maintenance while true
                // (daemon re-stamps on rotation), a dying credential while
                // false. Readers (ccsbar) key their token-row rendering off
                // this so a fed token never displays as an expiring mint.
                "session_feed": p.session_feed,
                "provider": provider_label(p),
                "base_url": p.base_url,
                "tier": tier_label(p),
                // Additive (schema stays 1): the account email this profile's
                // login last authenticated as (identity-anchor email half,
                // backfilled by the /profile fetch) — lets readers show WHICH
                // account a profile holds, so a wrong-account capture is
                // visible at a glance instead of via forensics. OAuth-only,
                // matching the TUI's gate: an OAuth→API conversion keeps the
                // cached anchor, and that stale email must not surface.
                "account_email": if p.is_codex() {
                    // Codex identity comes from the stored id_token claims,
                    // not the claude-side profile_cache anchors.
                    codex_auth.as_ref().and_then(|a| a.email())
                } else {
                    p.is_oauth().then(|| {
                        crate::profile_cache::load_profile_cache::<String>(
                            name,
                            crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE,
                        )
                    }).flatten()
                },
                "has_live_session": crate::runtime::has_live_session(name),
                "auth_status": if p.is_codex() {
                    // Same value set as the claude leg (schema stays 1):
                    // broken = quarantined, expiring = stored access token
                    // past its JWT exp, else ok.
                    if config.is_auth_broken(name) {
                        "broken"
                    } else if codex_auth
                        .as_ref()
                        .and_then(|a| a.access_token_exp_ms())
                        .is_some_and(|exp| now as i64 >= exp)
                    {
                        "expiring"
                    } else {
                        "ok"
                    }
                } else {
                    auth_status_str(config, p, now as i64)
                },
                "fetch_status": fetch_status,
                // Additive (schema stays 1): true when the daemon distrusts this
                // reading as a deep-slot stuck RateLimited — readers dim it / show
                // a "stuck" cue instead of treating it as current truth. Always
                // false for the single-shot `status --json`.
                "stale": stale,
                "fetched_at": mtime_ms.map(iso_from_ms),
                "next_refresh_at": next_refresh_ms.map(iso_from_ms),
                "auto_start": p.auto_start,
                "bell_threshold": p.bell_threshold,
                "fallback": fallback_json(config, p),
                "windows": windows_json(name),
                "third_party": third_party,
                // Additive, codex-only (null on claude profiles): when the
                // stored snapshot was last captured/adopted. ccsbar
                // decodeIfPresent — pinned in docs/ccsbar/DESIGN.md.
                "codex_snapshot_at": codex_snapshot_ms.map(iso_from_ms),
                // Additive, codex-only (CDX-4 §0.16): codex's own limiter
                // verdict — which window (`primary`/`secondary`) rejected the
                // last request, from the session-log snapshot. Readers
                // cross-check the named window's resets_at (a lapsed window
                // clears the badge). Null on claude profiles and when no
                // verdict is recorded.
                "codex_rate_limit_reached": p.is_codex().then(|| {
                    load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE)
                        .and_then(|u| u.codex_rate_limit_reached)
                }).flatten(),
            })
        })
        .collect();

    serde_json::json!({
        "schema": SCHEMA_VERSION,
        "generated_at": iso_from_ms(now),
        // TECH-8: additive version + last-switch event (schema stays 1). Version is
        // always present (daemon + single-shot) so CLI↔daemon skew is detectable.
        "clauth_version": env!("CARGO_PKG_VERSION"),
        "last_switch": live.and_then(|s| s.last_switch).map(|ls| serde_json::json!({
            "from": ls.from,
            "to": ls.to,
            "at": iso_from_ms(ls.at_ms),
            "trigger": ls.trigger,
        })),
        "active_profile": config.state.active_profile.as_deref(),
        // Additive (schema stays 1): the codex-harness active slot — which
        // profile's chain lives in ~/.codex/auth.json. Independent of
        // active_profile (claude); null on claude-only installs.
        "active_codex_profile": config.state.active_codex_profile.as_deref(),
        "pending_switch": live.and_then(|s| s.pending_switch),
        // TECH-6: additive (schema stays 1 — ccsbar decodeIfPresent). Always
        // present so readers can `has("last_error")`; null until a drain records one.
        "last_error": live.and_then(|s| s.last_error).map(|(at, message)| serde_json::json!({
            "at": iso_from_ms(at),
            "message": message,
        })),
        // Wire key stays "wrap_off" (ccsbar reads it); the Rust field followed
        // upstream's switch_off_when_spent rename.
        "wrap_off": config.state.switch_off_when_spent,
        "weekly_switch_threshold": config.state.weekly_switch_threshold_pct(),
        // Additive (schema stays 1): whether the ACTIVE-side switch decision
        // projects on burn rate (issue #8-b upstream) instead of the static
        // threshold — readers rendering "would switch at N%" need to know.
        "burn_aware": config.state.burn_aware_switching,
        // Additive (schema stays 1): the daemon's own next-move forecast — the
        // single source of truth for every "would switch to X" string.
        "forecast": forecast_json(config),
        "refresh_interval_ms": interval_ms,
        // Ordered fallback-chain member names — the auto-switch order. Per-profile
        // `fallback.position`/`threshold`/`armed` carry the same order, but the flat
        // list lets the menu bar render the chain without sorting.
        "fallback_chain": config
            .state
            .fallback_chain
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>(),
        // Additive (schema stays 1): the CODEX chain, same shape (CDX-4).
        // Empty on codex-less installs; readers decodeIfPresent.
        "codex_fallback_chain": config
            .state
            .codex_fallback_chain
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>(),
        "profiles": profiles,
    })
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_status_json.rs"]
mod tests;
