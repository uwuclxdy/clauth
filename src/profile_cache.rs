//! Generic per-profile disk-cache IO.
//!
//! Both the OAuth usage layer (`usage/fetch.rs`) and the third-party provider
//! layer (`providers/mod.rs`) persist one JSON file per profile under the same
//! per-profile dir, with the same atomic-write + None-on-error semantics. This
//! module owns that shared IO once; the two layers only differ in their cache
//! filename and the concrete type.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::{Serialize, de::DeserializeOwned};

/// Filename of the OAuth usage cache, relative to the per-profile dir.
pub(crate) const USAGE_CACHE_FILE: &str = "usage_cache.json";
/// Filename of the third-party provider cache, relative to the per-profile dir.
pub(crate) const THIRD_PARTY_CACHE_FILE: &str = "third_party_cache.json";

/// The account uuid this profile's login last authenticated as (a bare JSON
/// string). Derived data, backfilled on login and on every successful mirror
/// adoption — the identity anchor that lets an unattended adopt refuse a live
/// login belonging to a DIFFERENT account (`oauth::try_adopt_live_rotation`).
pub(crate) const ACCOUNT_ID_CACHE_FILE: &str = "account_id.json";

/// The account EMAIL paired with [`ACCOUNT_ID_CACHE_FILE`] — the operator-
/// readable half of the identity anchor (surfaced in the TUI Setup tab,
/// `status.json`, and the daemon's same-account tripwire). Written/dropped
/// wherever the uuid anchor moves; backfilled by the same `/profile` fetch.
pub(crate) const ACCOUNT_EMAIL_CACHE_FILE: &str = "account_email.json";

/// Epoch-ms of this profile's last `/profile` fetch attempt (a bare JSON number).
/// Derived data: the durable half of `usage::fetch`'s once-per-hour-per-profile
/// TTL clock, so a relaunch reuses the cached plan instead of re-pulling
/// `/profile` for every profile at once.
pub(crate) const PROFILE_FETCHED_CACHE_FILE: &str = "profile_fetched.json";

/// Per-profile kick-429 block (`usage::scheduler::KickBlock`): written by the
/// fetching instance so a standdown TUI can mirror the judgment and a restart
/// doesn't forget a live block mid-outage; removed the moment a kick lands.
pub(crate) const KICK_BLOCK_CACHE_FILE: &str = "kick_block.json";

/// CDX-6: the codex plan tier (`pro`/`plus`/`free`/…) as the LIVE backend
/// last reported it (`wham/usage` top-level `plan_type`). The stored
/// id_token's `chatgpt_plan_type` claim goes stale the moment the account
/// upgrades (it only re-mints when codex itself refreshes) — `tier_label`
/// prefers this cache over the claim so an upgrade shows within a poll
/// interval (AX report 2026-07-22: ax-codex-cl upgraded plus→pro, label
/// stuck on plus).
pub(crate) const CODEX_PLAN_CACHE_FILE: &str = "codex_plan.json";

/// Resolve `<profile_dir>/<file>` for `name`. `None` only when the per-profile
/// dir itself can't be resolved (matches the prior per-layer `cache_path`).
pub(crate) fn profile_cache_path(name: &str, file: &str) -> Option<PathBuf> {
    // `profile_dir` (override-aware) rather than raw `dirs::home_dir`, so tests
    // never touch the real `~/.clauth`.
    crate::profile::profile_dir(name).ok().map(|p| p.join(file))
}

/// Read + deserialize `<profile_dir>/<file>`. `None` on missing file or any
/// read/parse error — the caller treats both as "no cache" (matches the prior
/// per-layer loaders exactly).
pub(crate) fn load_profile_cache<T: DeserializeOwned>(name: &str, file: &str) -> Option<T> {
    profile_cache_path(name, file).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<T>(&text).ok()
    })
}

/// Atomically write `value` to `<profile_dir>/<file>`. Failures are swallowed
/// (cache is best-effort): a missing parent is created at 0o700, the file at
/// 0o600, via a tmp + rename so a torn write reads as no cache rather than a
/// parse failure.
pub(crate) fn write_profile_cache<T: Serialize>(name: &str, file: &str, value: &T) {
    let Some(path) = profile_cache_path(name, file) else {
        return;
    };
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    let _ = crate::profile::atomic_write_600(&path, json.as_bytes());
}

/// CAP-1: keep the identity anchor coherent with a sanctioned live-credential
/// capture — the anchor must move with the store. The captured login's uuid
/// comes from CC's own `~/.claude.json` `oauthAccount` block when present;
/// when it is absent/mid-write the stale anchor is DROPPED rather than left
/// lying. A wrong anchor silently re-routes the identity-guarded adopt/follow
/// paths (2026-07-12: a profile held a sibling's chain behind its own stale
/// anchor), while a missing one makes them refuse until the hourly `/profile`
/// fetch re-backfills it — refuse-and-heal beats trusting a lie.
pub(crate) fn refresh_account_anchor(name: &str) {
    // ONE read of ~/.claude.json for both halves — two independent reads
    // could straddle a rewrite and pair one account's uuid with another's
    // email (the exact split this pair exists to prevent).
    match crate::claude_json::live_oauth_account_pair() {
        Some((uuid, email)) => {
            write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &uuid);
            // The email moves (or drops) in lockstep with the uuid so the
            // anchor pair can never describe two different accounts.
            match email {
                Some(email) => write_profile_cache(name, ACCOUNT_EMAIL_CACHE_FILE, &email),
                None => drop_cache_file(name, ACCOUNT_EMAIL_CACHE_FILE),
            }
        }
        None => drop_account_anchor(name),
    }
}

/// Remove `name`'s identity anchor pair (no login → no identity to anchor).
/// Email FIRST: a torn drop (crash/unlink failure between the two) must leave
/// uuid-present + email-absent — harmless, later re-seeded under an agreeing
/// uuid — never a surviving email the backfill would pair with a NEW uuid.
pub(crate) fn drop_account_anchor(name: &str) {
    drop_cache_file(name, ACCOUNT_EMAIL_CACHE_FILE);
    drop_cache_file(name, ACCOUNT_ID_CACHE_FILE);
}

pub(crate) fn drop_cache_file(name: &str, file: &str) {
    if let Some(path) = profile_cache_path(name, file) {
        let _ = std::fs::remove_file(path);
    }
}

/// Delete `<profile_dir>/<file>`. Best-effort, same contract as the writer: an
/// already-absent file and any removal error alike leave the caller with "no
/// cache", which is the intended post-state either way. Upstream's name for
/// [`drop_cache_file`], kept as a delegating alias so upstream call sites merge
/// without churn.
pub(crate) fn remove_profile_cache(name: &str, file: &str) {
    drop_cache_file(name, file);
}

/// Epoch-ms of `<profile_dir>/<file>`'s last write, or `None` when it's absent.
pub(crate) fn profile_cache_mtime_ms(name: &str, file: &str) -> Option<u64> {
    let modified = std::fs::metadata(profile_cache_path(name, file)?)
        .ok()?
        .modified()
        .ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}
