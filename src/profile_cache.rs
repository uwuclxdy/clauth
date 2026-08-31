//! Generic per-profile disk-cache IO.
//!
//! Both the OAuth usage layer (`usage/fetch.rs`) and the third-party provider
//! layer (`providers/mod.rs`) persist one JSON file per profile under the same
//! per-profile dir, with the same atomic-write + None-on-error semantics. This
//! module owns that shared IO once; the two layers only differ in their cache
//! filename and the concrete type.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::profile::ProfileName;

/// Filename of the OAuth usage cache, relative to the per-profile dir.
pub(crate) const USAGE_CACHE_FILE: &str = "usage_cache.json";
/// Filename of the third-party provider cache, relative to the per-profile dir.
pub(crate) const THIRD_PARTY_CACHE_FILE: &str = "third_party_cache.json";

/// The account uuid this profile's login last authenticated as (a bare JSON
/// string). Derived data, backfilled on login and on every successful mirror
/// adoption — the identity anchor that lets an unattended adopt refuse a live
/// login belonging to a DIFFERENT account (`oauth::try_adopt_live_rotation`).
pub(crate) const ACCOUNT_ID_CACHE_FILE: &str = "account_id.json";

/// Epoch-ms of this profile's last `/profile` fetch attempt (a bare JSON number).
/// Derived data: the durable half of `usage::fetch`'s once-per-hour-per-profile
/// TTL clock, so a relaunch reuses the cached plan instead of re-pulling
/// `/profile` for every profile at once.
pub(crate) const PROFILE_FETCHED_CACHE_FILE: &str = "profile_fetched.json";

/// Per-profile kick-429 block (`usage::scheduler::KickBlock`): written by the
/// fetching instance so a standdown TUI can mirror the judgment and a restart
/// doesn't forget a live block mid-outage; removed the moment a kick lands.
pub(crate) const KICK_BLOCK_CACHE_FILE: &str = "kick_block.json";

/// The last third-party fetch for this profile died on a credential that can
/// never self-heal ([`crate::usage::FetchStatus::AuthExpired`]), recorded
/// against the fingerprint of the credential that produced it.
///
/// It exists for the surfaces with no scheduler in the process — `clauth list`
/// and `clauth status --json` — which otherwise derive freshness from the usage
/// cache's mtime alone and so report a warm cache behind a dead session as
/// `Fresh`: a live measurement, over a credential that will never come back.
///
/// This is NOT a freshness claim and cannot go stale in the dangerous
/// direction: it is a terminal verdict BOUND to one credential, so a re-login
/// changes the fingerprint and the record stops applying on its own — the same
/// invalidation rule the scheduler's in-memory suppression uses.
pub(crate) const THIRD_PARTY_AUTH_FILE: &str = "third_party_auth.json";

/// Body of [`THIRD_PARTY_AUTH_FILE`]. A struct rather than a bare number so a
/// second field can join without a format break.
#[derive(Debug, Serialize, Deserialize)]
struct AuthVerdict {
    /// `ThirdPartyEntry::credential_fingerprint` of the credential that failed.
    credential: u64,
}

/// Record that `name`'s usage credential is dead. Best-effort like every other
/// per-profile cache: a record that never lands leaves the reader on the
/// mtime derivation, which is the pre-record answer rather than a worse one.
pub(crate) fn write_auth_expired(name: &ProfileName, credential: u64) {
    write_profile_cache(name, THIRD_PARTY_AUTH_FILE, &AuthVerdict { credential });
}

/// Drop the record — any outcome other than `AuthExpired` proves the verdict no
/// longer holds. Cheap and unconditional: the file is usually absent.
pub(crate) fn clear_auth_expired(name: &ProfileName) {
    remove_profile_cache(name, THIRD_PARTY_AUTH_FILE);
}

/// Whether `name` carries a dead-credential verdict for the credential it holds
/// RIGHT NOW. A record for any other fingerprint is inert — that is what makes
/// this safe to persist.
pub(crate) fn auth_expired_matches(name: &ProfileName, credential: u64) -> bool {
    load_profile_cache::<AuthVerdict>(name, THIRD_PARTY_AUTH_FILE)
        .is_some_and(|v| v.credential == credential)
}

/// The one credential-store mtime bump clauth makes with NO bytes behind it
/// ([`TouchReceipt`]). Sits beside the store it describes, so
/// [`effective_write_time`] resolves it from the store's own path.
/// Per-profile parked MCP-server logins (`claude::park_mcp_logins_from_store`),
/// written only while a profile stores no credentials file of its own to hold
/// them. Carries none of the account's own credential material: the blocks
/// inside are minted per (MCP server, endpoint) and belong to no Claude account,
/// which is why a profile that stops storing a login still has to keep them.
pub(crate) const MCP_LOGINS_FILE: &str = "mcp-logins.json";

pub(crate) const TOUCH_RECEIPT_FILE: &str = "touch-receipt.json";

/// A store mtime clauth moved without writing the store.
///
/// The per-session swap executor must move the mtime of the store it repoints to
/// — Claude Code re-reads credentials only when that value changes, so an
/// mtime-preserving repoint is a silent no-op — but every OTHER reader of that
/// mtime is asking when the bytes were last WRITTEN, and answers a bare stamp as
/// "just now". This is what separates the two.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TouchReceipt {
    /// File name of the stamped store. A profile can hold both a
    /// `credentials.json` and a `session-token.json` and only ever one of them
    /// is stamped, so a receipt for one must not resolve the other.
    store: String,
    /// The mtime the stamp actually landed on, read back rather than predicted —
    /// a coarse filesystem truncates the value asked for. A store whose mtime has
    /// moved off this has been genuinely written since, which retires the
    /// receipt.
    stamped: SystemTime,
    /// The mtime the stamp displaced: when the store's bytes were last written.
    prev: Option<SystemTime>,
}

/// Record that `name`'s `store` carries a bare stamp. Best-effort like every
/// other per-profile cache — a receipt that never lands leaves the readers on the
/// raw mtime, which is the pre-receipt answer rather than a worse one.
pub(crate) fn write_touch_receipt(
    name: &ProfileName,
    store: &Path,
    stamped: SystemTime,
    prev: Option<SystemTime>,
) {
    let Some(file) = store.file_name().and_then(|f| f.to_str()) else {
        return;
    };
    debug_assert!(
        crate::profile::profile_dir(name).ok().as_deref() == store.parent(),
        "the receipt is read back from the store's own directory, so it has to be written there",
    );
    write_profile_cache(
        name,
        TOUCH_RECEIPT_FILE,
        &TouchReceipt {
            store: file.to_string(),
            stamped,
            prev,
        },
    );
}

/// When `store`'s bytes were last written: its mtime, unless a [`TouchReceipt`]
/// beside it identifies that exact mtime as a bare stamp — in which case the
/// value that stamp displaced.
///
/// Every decision that compares store mtimes to answer "which side was written
/// last" resolves through here. A receipt that is absent, unreadable,
/// unparseable, for the other store, or retired by a later write yields the raw
/// mtime, so a lost or stale receipt costs exactly the pre-receipt answer.
pub(crate) fn effective_write_time(store: &Path) -> Option<SystemTime> {
    // Returns before the receipt read when the store is absent: nothing can have
    // displaced a write that never happened, and a profile with no sidecar is the
    // common case on the reload fingerprint's per-profile walk.
    let mtime = std::fs::metadata(store)
        .ok()
        .and_then(|m| m.modified().ok())?;
    let Some(file) = store.file_name().and_then(|f| f.to_str()) else {
        return Some(mtime);
    };
    std::fs::read_to_string(store.with_file_name(TOUCH_RECEIPT_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<TouchReceipt>(&text).ok())
        .filter(|receipt| receipt.store == file && mtime == receipt.stamped)
        .map_or(Some(mtime), |receipt| receipt.prev)
}

/// Resolve `<profile_dir>/<file>` for `name`. `None` only when the per-profile
/// dir itself can't be resolved (matches the prior per-layer `cache_path`).
pub(crate) fn profile_cache_path(name: &ProfileName, file: &str) -> Option<PathBuf> {
    // `profile_dir` (override-aware) rather than raw `dirs::home_dir`, so tests
    // never touch the real `~/.clauth`.
    crate::profile::profile_dir(name).ok().map(|p| p.join(file))
}

/// Read + deserialize `<profile_dir>/<file>`. `None` on missing file or any
/// read/parse error — the caller treats both as "no cache" (matches the prior
/// per-layer loaders exactly).
pub(crate) fn load_profile_cache<T: DeserializeOwned>(name: &ProfileName, file: &str) -> Option<T> {
    profile_cache_path(name, file).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<T>(&text).ok()
    })
}

/// Atomically write `value` to `<profile_dir>/<file>`. Failures are swallowed
/// (cache is best-effort): a missing parent is created at 0o700, the file at
/// 0o600, via a tmp + rename so a torn write reads as no cache rather than a
/// parse failure.
///
/// Skips names the on-disk record no longer carries: the fetch legs hold a
/// stale in-memory config for up to a tick, and the parent creation above
/// would re-create a deleted profile's directory. Fail-closed — an unreadable
/// record skips the write too. The tick-driven callers (usage fetch,
/// scheduler) retry on the next tick; one-shot writers degrade to their
/// documented safe answers instead (a lost touch receipt yields the raw
/// mtime, an unseeded anchor reads as unanchored). Cost: one read + TOML
/// parse of `profiles.toml` per write — the record is small, and the same
/// read already backs the oauth adopt gate and the daemon's switch-target
/// existence check.
///
/// "The record" means EITHER roster. The gate exists to skip names that are no
/// longer accounts, and a codex profile is an account — it simply lives in
/// `codex-profiles.toml`, which `is_configured` cannot see (it reads
/// `profiles.toml` alone, by design). Checking only that one silently dropped
/// every codex usage reading, so the cache the published feed and the Overview
/// resolve BY NAME stayed empty forever while the fetch itself succeeded.
pub(crate) fn write_profile_cache<T: Serialize>(name: &ProfileName, file: &str, value: &T) {
    let configured = crate::profile::is_configured(name).unwrap_or(false)
        || crate::codex_profiles::CodexState::load().is_ok_and(|s| s.holds(name.as_str()));
    if !configured {
        return;
    }
    let Some(path) = profile_cache_path(name, file) else {
        return;
    };
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    let _ = crate::profile::atomic_write_600(&path, json.as_bytes());
}

/// Delete `<profile_dir>/<file>`. Best-effort, same contract as the writer: an
/// already-absent file and any removal error alike leave the caller with "no
/// cache", which is the intended post-state either way.
pub(crate) fn remove_profile_cache(name: &ProfileName, file: &str) {
    if let Some(path) = profile_cache_path(name, file) {
        let _ = std::fs::remove_file(path);
    }
}

/// Epoch-ms of `<profile_dir>/<file>`'s last write, or `None` when it's absent.
pub(crate) fn profile_cache_mtime_ms(name: &ProfileName, file: &str) -> Option<u64> {
    let modified = std::fs::metadata(profile_cache_path(name, file)?)
        .ok()?
        .modified()
        .ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}
