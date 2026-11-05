//! Generic per-profile disk-cache IO.
//!
//! Both the OAuth usage layer (`usage/fetch.rs`) and the third-party provider
//! layer (`providers/mod.rs`) persist one JSON file per profile under the same
//! per-profile dir, with the same atomic-write + None-on-error semantics. This
//! module owns that shared IO once; the two layers only differ in their cache
//! filename and the concrete type.

use std::any::TypeId;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::harness::Harness;
use crate::profile::ProfileName;

/// Filename of the OAuth usage cache, relative to the per-profile dir.
pub(crate) const USAGE_CACHE_FILE: &str = "usage_cache.json";
/// Filename of the third-party provider cache, relative to the per-profile dir.
pub(crate) const THIRD_PARTY_CACHE_FILE: &str = "third_party_cache.json";
/// Filename of the claude banked-reset side read's cache, relative to the
/// per-profile dir. Written by the scheduler's hourly side read alone; the
/// plain poll never touches it.
pub(crate) const LIMIT_RESETS_FILE: &str = "limit_resets.json";

/// The account uuid this profile's login last authenticated as (a bare JSON
/// string). Derived data, backfilled on login and on every successful mirror
/// adoption — the identity anchor that lets an unattended adopt refuse a live
/// login belonging to a DIFFERENT account (`oauth::try_adopt_live_rotation`).
pub(crate) const ACCOUNT_ID_CACHE_FILE: &str = "account_id.json";

/// The last adopt refusal announced for this profile, as one of the reason
/// keys `oauth::try_adopt_live_rotation` refuses with (a bare JSON string).
/// The refusal is a STANDING state while the live slot stays unadoptable, and
/// the rotation legs that reach it run in DIFFERENT processes (the daemon
/// scheduler and the TUI's in-process scheduler), so the once-per-state
/// announcement is recorded on disk where every process reads it instead of
/// in either process's memory.
pub(crate) const ADOPT_REFUSAL_FILE: &str = "adopt_refusal.json";

/// Epoch-ms of this profile's last `/profile` fetch attempt (a bare JSON number).
/// Derived data: the durable half of `usage::fetch`'s once-per-hour-per-profile
/// TTL clock, so a relaunch reuses the cached plan instead of re-pulling
/// `/profile` for every profile at once.
pub(crate) const PROFILE_FETCHED_CACHE_FILE: &str = "profile_fetched.json";

/// Per-profile kick-429 block (`usage::scheduler::KickBlock`): written by the
/// fetching instance so a standdown TUI can mirror the judgment and a restart
/// doesn't forget a live block mid-outage; removed the moment a kick lands.
pub(crate) const KICK_BLOCK_CACHE_FILE: &str = "kick_block.json";

/// The last third-party fetch for this profile, or the last usage probe of its
/// setup token, died on a credential that can never self-heal
/// ([`crate::usage::FetchStatus::AuthExpired`]), recorded against the
/// fingerprint of the credential that produced it: the USAGE credential, which
/// on Alibaba is the console session and everywhere else the api key, or a
/// pairless account's setup-token digest
/// ([`crate::usage::dead_login_fingerprint`]).
///
/// It exists for the surfaces with no scheduler in the process — `clauth list`
/// and `clauth status --json` — which otherwise derive freshness from the usage
/// cache's mtime alone and so report a warm cache behind a dead session as
/// `Fresh`: a live measurement, over a credential that will never come back.
/// The refusal splitter `oauth::third_party_dead_chain_copy` is a third reader:
/// a matched verdict on an Alibaba profile renders the dead-console sentence.
/// Whether the api KEY is dead is [`DEAD_KEY_FILE`]'s question, not this one's.
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
    /// [`crate::usage::dead_login_fingerprint`] of the credential that failed:
    /// a third-party entry's credential fingerprint, or a setup token's digest.
    credential: u64,
}

/// The provider refused this profile's api KEY — the inference credential its
/// sessions and delegates spend — recorded against
/// [`crate::usage::inference_key_fingerprint`], a digest of that key alone.
/// Written by a delegate's 401 on the key and by a usage fetch that
/// authenticated with the key and got `AuthExpired`; cleared only by a live
/// body from a fetch that authenticated with the same key
/// ([`clear_auth_expired`]). A console re-login, a console lapse and an
/// endpoint edit never touch it, and a re-keyed profile leaves it inert. Read
/// by [`crate::fallback::third_party_key_rejected`].
pub(crate) const DEAD_KEY_FILE: &str = "dead_key.json";

/// Body of [`DEAD_KEY_FILE`].
#[derive(Debug, Serialize, Deserialize)]
struct DeadKey {
    /// [`crate::usage::inference_key_fingerprint`] of the refused key.
    key: u64,
}

/// Record that `name`'s usage credential is dead. Best-effort like every other
/// per-profile cache: a record that never lands leaves the reader on the
/// mtime derivation, which is the pre-record answer rather than a worse one.
pub(crate) fn write_auth_expired(name: &ProfileName, credential: u64) {
    write_profile_cache_for(
        Harness::Claude,
        name,
        THIRD_PARTY_AUTH_FILE,
        &AuthVerdict { credential },
    );
}

/// Drop both records — a live body from a fetch that authenticated with the
/// api key proves the usage credential AND the key alive. Cheap and
/// unconditional: the files are usually absent.
pub(crate) fn clear_auth_expired(name: &ProfileName) {
    clear_usage_verdict(name);
    remove_profile_cache(name, DEAD_KEY_FILE);
}

/// Drop the usage-credential record alone — a live body from a fetch that never
/// read the api key (Alibaba's console-only quota) proves nothing about the key.
pub(crate) fn clear_usage_verdict(name: &ProfileName) {
    remove_profile_cache(name, THIRD_PARTY_AUTH_FILE);
}

/// Whether `name` carries a dead-credential verdict for the credential it holds
/// RIGHT NOW. A record for any other fingerprint is inert — that is what makes
/// this safe to persist.
pub(crate) fn auth_expired_matches(name: &ProfileName, credential: u64) -> bool {
    load_profile_cache::<AuthVerdict>(name, THIRD_PARTY_AUTH_FILE)
        .is_some_and(|v| v.credential == credential)
}

/// Record that the provider refused `name`'s api key, `key` being its
/// [`crate::usage::inference_key_fingerprint`]. Best-effort like
/// [`write_auth_expired`].
pub(crate) fn write_dead_key(name: &ProfileName, key: u64) {
    write_profile_cache_for(Harness::Claude, name, DEAD_KEY_FILE, &DeadKey { key });
}

/// Whether `name`'s dead-key record names the key it holds RIGHT NOW. A record
/// for any other key is inert.
pub(crate) fn dead_key_matches(name: &ProfileName, key: u64) -> bool {
    load_profile_cache::<DeadKey>(name, DEAD_KEY_FILE).is_some_and(|v| v.key == key)
}

/// This member answered a session with Claude Code's spend-cap 429 (`apiError:
/// "model_requires_usage_credits"`): it refused a request billed against usage
/// credits. Written by the instance that fetches usage when it drains a
/// spend-cap 429, and by the lift of an isolated run's tree; read in every
/// process, so a restart or a lease handover keeps it: the session walks skip
/// the member for a demand that may need credits until every other pass is
/// spent, the global walk in its headroom and home passes. It goes on any later
/// successful turn on the member ([`CreditRefusal`]), on a reading that shows a
/// window reset after it with the member back under its lines
/// ([`crate::fallback::credit_refusal_reset_at`]), on a login as another account
/// or a `/profile` read naming one, or with the profile; never by age alone,
/// since the cap resets with a billing month clauth does not know.
pub(crate) const CREDIT_REFUSAL_CACHE_FILE: &str = "credit_refusal.json";

/// A credit refusal on record ([`CREDIT_REFUSAL_CACHE_FILE`]). `at_ms` and
/// `file` are required, so a foreign file fails the read rather than parse as
/// a refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CreditRefusal {
    /// The 429 row's own instant, epoch ms.
    pub(crate) at_ms: u64,
    /// The transcript the 429 came from: a successful turn on the member in
    /// that file is ordered against the refusal by line, one in any other file
    /// by stamp.
    pub(crate) file: PathBuf,
    /// The 429 line's `uuid`, when it carries one: the tps producer's record of
    /// where that line lies in `file` is pulled by it, so a turn in that file
    /// is ordered against the refusal by line rather than by stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) uuid: Option<String>,
}

/// Body of [`CREDIT_REFUSAL_CACHE_FILE`]: the refusal on record, and the floor
/// a login change or a clear leaves. Read through [`credit_refusal`] alone, so
/// every reader takes a record that carries only the floor for no refusal.
/// A key this build does not know is ignored, as [`AuthVerdict`] ignores one,
/// so a record a newer clauth wrote with one more field still reads and the
/// next write here keeps its floor.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CreditRefusalRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refusal: Option<CreditRefusal>,
    /// A refusal at or before this instant, epoch ms, is never recorded again:
    /// it answered a login the account has since left, or it is stamped no
    /// later than a refusal a later turn cleared, or than that turn. A lease
    /// holder that takes over re-reads the hot transcripts whole, the refused
    /// session's 429 included, and the turn that cleared it may be gone from
    /// what it reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floor_ms: Option<u64>,
    /// The account the identity anchor named when the refusal went on record:
    /// a `/profile` read naming another one drops it whether or not the anchor
    /// still stands ([`forget_credit_refusal_of_another_account`]). Absent for
    /// a refusal recorded with no anchor on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<crate::profile::AccountId>,
}

fn credit_refusal_record(name: &ProfileName) -> CreditRefusalRecord {
    load_profile_cache(name, CREDIT_REFUSAL_CACHE_FILE).unwrap_or_default()
}

/// The account `name`'s identity anchor names, read for the record a refusal
/// goes on.
fn anchored_account(name: &ProfileName) -> Option<crate::profile::AccountId> {
    load_profile_cache::<crate::profile::AccountId>(name, ACCOUNT_ID_CACHE_FILE)
        .filter(|u| !u.trim().is_empty())
}

/// The credit refusal on record for `name`, if any: the one gate every reader
/// goes through.
pub(crate) fn credit_refusal(name: &ProfileName) -> Option<CreditRefusal> {
    credit_refusal_record(name).refusal
}

/// Record `refusal` for `name`, unless the one on record is as new, two
/// sessions refused on one member keeping the later 429, or it is no newer
/// than the floor on record.
///
/// The read and the write hold the state flock, which a login change's forget
/// ([`forget_credit_refusal`]) runs under, so a forget from another process
/// cannot land between them and lose its floor. A flock wait that times out
/// records nothing and logs it: the 429 is lost, and the member's next one
/// places it again. The hold shares the tick's `timed_out` flag: once any
/// landing or kick hold of the tick has timed out, it takes that path at once
/// rather than wait out the flock again.
pub(crate) fn write_credit_refusal_flagged(
    name: &ProfileName,
    refusal: &CreditRefusal,
    timed_out: Option<&std::sync::atomic::AtomicBool>,
) {
    let held = crate::lock::with_state_lock_flag(timed_out, |_held| {
        let mut record = credit_refusal_record(name);
        #[cfg(test)]
        credit_refusal_park::park_if_armed();
        if record.take(refusal, anchored_account(name)) {
            write_profile_cache_for(Harness::Claude, name, CREDIT_REFUSAL_CACHE_FILE, &record);
        }
        Ok(())
    });
    if let Err(e) = held {
        crate::logline::logline!("clauth: {name}: could not record its credit refusal: {e}");
    }
}

/// [`write_credit_refusal_flagged`] outside a tick.
#[cfg(test)]
pub(crate) fn write_credit_refusal(name: &ProfileName, refusal: &CreditRefusal) {
    write_credit_refusal_flagged(name, refusal, None);
}

/// Drop `name`'s credit refusal, a later successful turn on the member or a
/// window reset after it having cleared it, and floor the record at
/// `floor_ms`: for a turn the later of the cleared refusal's instant and the
/// clearing turn's stamp, for a reset the reset's instant. A re-read of that 429
/// carries the first, and a second refusal stamped before the turn but drained
/// after the clear sits below the second, so neither is recorded again. A
/// higher floor on record stays, and so does a refusal past `floor_ms`, which
/// another session recorded after the caller read the one the turn cleared.
/// Held under the state flock like [`write_credit_refusal_flagged`], and a
/// timed-out wait clears nothing.
pub(crate) fn clear_credit_refusal_flagged(
    name: &ProfileName,
    floor_ms: u64,
    timed_out: Option<&std::sync::atomic::AtomicBool>,
) {
    let held = crate::lock::with_state_lock_flag(timed_out, |_held| {
        let mut record = credit_refusal_record(name);
        record.clear(floor_ms);
        write_profile_cache_for(Harness::Claude, name, CREDIT_REFUSAL_CACHE_FILE, &record);
        Ok(())
    });
    if let Err(e) = held {
        crate::logline::logline!("clauth: {name}: could not clear its credit refusal: {e}");
    }
}

/// [`clear_credit_refusal_flagged`] outside a tick.
#[cfg(test)]
pub(crate) fn clear_credit_refusal(name: &ProfileName, floor_ms: u64) {
    clear_credit_refusal_flagged(name, floor_ms, None);
}

/// Record `refusal` for `name` as [`write_credit_refusal_flagged`] does, then
/// clear the refusal that stands on record as [`clear_credit_refusal_flagged`]
/// does when `served_past` names the stamp of a successful turn on the member
/// past it, for a caller that holds every turn of a run at once. Both run in
/// one state-flock hold, so a refusal another process records in between is
/// ordered against the same turns rather than outliving them.
pub(crate) fn settle_credit_refusal(
    name: &ProfileName,
    refusal: Option<&CreditRefusal>,
    served_past: impl FnOnce(&CreditRefusal) -> Option<u64>,
) {
    let held = crate::lock::with_state_lock(|_held| {
        let mut record = credit_refusal_record(name);
        let mut changed =
            refusal.is_some_and(|refusal| record.take(refusal, anchored_account(name)));
        if let Some(held) = record.refusal.clone()
            && let Some(turn_ts) = served_past(&held)
        {
            record.clear(held.at_ms.max(turn_ts));
            changed = true;
        }
        if changed {
            write_profile_cache_for(Harness::Claude, name, CREDIT_REFUSAL_CACHE_FILE, &record);
        }
        Ok(())
    });
    if let Err(e) = held {
        crate::logline::logline!("clauth: {name}: could not settle its credit refusal: {e}");
    }
}

impl CreditRefusalRecord {
    /// Take `refusal`, placed while the anchor named `account`, unless the one
    /// held is as new or the floor lies at or past it
    /// ([`write_credit_refusal_flagged`]); whether it took it.
    fn take(
        &mut self,
        refusal: &CreditRefusal,
        account: Option<crate::profile::AccountId>,
    ) -> bool {
        if self.floor_ms.is_some_and(|floor| refusal.at_ms <= floor)
            || self
                .refusal
                .as_ref()
                .is_some_and(|held| held.at_ms >= refusal.at_ms)
        {
            return false;
        }
        self.refusal = Some(refusal.clone());
        self.account = account;
        true
    }

    /// Drop the refusal held unless it lies past `floor_ms`, and floor the
    /// record at `floor_ms`, a higher floor held staying
    /// ([`clear_credit_refusal_flagged`]).
    fn clear(&mut self, floor_ms: u64) {
        if self
            .refusal
            .as_ref()
            .is_some_and(|held| held.at_ms <= floor_ms)
        {
            self.refusal = None;
            self.account = None;
        }
        self.floor_ms = Some(self.floor_ms.map_or(floor_ms, |held| held.max(floor_ms)));
    }
}

// Test seam parking the next credit-refusal write between its read and its
// write, inside its state-flock hold: a test arms it, the writer signals
// `parked` and blocks until the test sends on `release`, so a second process's
// forget can be ordered against the hold by channel, never wall time. Inert
// while unarmed; a dropped `Park` clears an un-consumed arm.
#[cfg(test)]
pub(crate) mod credit_refusal_park {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender};

    /// A park the test holds: `parked` fires once the armed write reaches its
    /// park point; `release` unblocks it.
    pub(crate) struct Park {
        pub(crate) parked: Receiver<()>,
        pub(crate) release: Sender<()>,
    }

    static ARMED: Mutex<Option<(Receiver<()>, Sender<()>)>> = Mutex::new(None);

    /// Arm the seam for the next credit-refusal write.
    pub(crate) fn arm() -> Park {
        let (release, release_rx) = std::sync::mpsc::channel();
        let (parked, parked_rx) = std::sync::mpsc::channel();
        let mut armed = ARMED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(armed.is_none(), "a credit-refusal park is already armed");
        *armed = Some((release_rx, parked));
        Park {
            parked: parked_rx,
            release,
        }
    }

    pub(super) fn park_if_armed() {
        let (release_rx, parked) = match ARMED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(armed) => armed,
            None => return,
        };
        let _ = parked.send(());
        let _ = release_rx.recv();
    }

    impl Drop for Park {
        fn drop(&mut self) {
            ARMED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
    }
}

/// Drop `name`'s credit refusal for a login change and floor the record at
/// `now_ms`: a refusal from before answered the login the account left, and
/// nothing may record it again. Deleting the file by hand drops the floor too,
/// a full reset. A profile with no directory gets no record.
pub(crate) fn forget_credit_refusal(name: &ProfileName, now_ms: u64) {
    let dir_exists = profile_cache_path(name, CREDIT_REFUSAL_CACHE_FILE)
        .as_deref()
        .and_then(Path::parent)
        .is_some_and(Path::is_dir);
    if !dir_exists {
        return;
    }
    write_profile_cache_for(
        Harness::Claude,
        name,
        CREDIT_REFUSAL_CACHE_FILE,
        &CreditRefusalRecord {
            refusal: None,
            floor_ms: Some(now_ms),
            account: None,
        },
    );
}

/// Drop `name`'s credit refusal as [`forget_credit_refusal`] does when it went
/// on record for another account than `account`, the one a `/profile` read
/// just named. The record's own account decides, not the anchor: an install
/// that proves no uuid removes the anchor, and a refusal from the account the
/// profile ran as before must go all the same. A refusal recorded with no
/// anchor names no account and stays. Called inside the read's state-flock
/// hold.
pub(crate) fn forget_credit_refusal_of_another_account(
    name: &ProfileName,
    account: &crate::profile::AccountId,
    now_ms: u64,
) {
    let record = credit_refusal_record(name);
    if record.refusal.is_some()
        && record
            .account
            .as_ref()
            .is_some_and(|placed| placed.trim() != account.trim())
    {
        forget_credit_refusal(name, now_ms);
    }
}

/// Each profile's credit refusal on record, re-read for a profile only when
/// its record's stat moves (an mtime move, or the file appearing or going), so
/// a pass costs one stat per profile. The TUI's mirror and the daemon's log
/// lines read it; the instance that fetches usage writes and clears the
/// records, possibly in another process.
#[derive(Debug, Default)]
pub(crate) struct CreditRefusalWatch {
    seen: std::collections::HashMap<ProfileName, (Option<SystemTime>, Option<CreditRefusal>)>,
}

impl CreditRefusalWatch {
    /// Re-read the records of `names` whose stat moved, and forget the names
    /// no longer listed. Returns each name whose refusal came on record or
    /// went this pass, with whether it is on record now; a name seen for the
    /// first time with a refusal counts as come.
    pub(crate) fn refresh(&mut self, names: &[ProfileName]) -> Vec<(ProfileName, bool)> {
        self.seen.retain(|name, _| names.contains(name));
        let mut edges = Vec::new();
        for name in names {
            let stat = profile_cache_path(name, CREDIT_REFUSAL_CACHE_FILE)
                .and_then(|path| std::fs::metadata(path).ok())
                .and_then(|meta| meta.modified().ok());
            let was = match self.seen.get(name) {
                Some((seen, _)) if *seen == stat => continue,
                Some((_, refusal)) => refusal.is_some(),
                None => false,
            };
            let refusal = credit_refusal(name);
            let now = refusal.is_some();
            self.seen.insert(name.clone(), (stat, refusal));
            if now != was {
                edges.push((name.clone(), now));
            }
        }
        edges
    }

    /// The refusals on record as of the last pass.
    pub(crate) fn refusals(&self) -> std::collections::HashMap<ProfileName, CreditRefusal> {
        self.seen
            .iter()
            .filter_map(|(name, (_, refusal))| refusal.clone().map(|r| (name.clone(), r)))
            .collect()
    }
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
    write_profile_cache_for(
        Harness::Claude,
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

/// The cache file each usage-cache payload type is written to, so a call site
/// pairing the wrong constant with the wrong type fails the read instead of
/// deserializing a foreign file into an all-default value. An all-`serde(default)`
/// payload (e.g. [`crate::usage::UsageInfo`]) parses ANY JSON object into a
/// valid-looking all-default answer, so the constant↔type mismatch is invisible
/// to a parse and no test can catch it — the defect a two-part fix once shipped
/// half-swallowed by. Payloads with required fields fail a mismatched read on
/// their own (`ThirdPartyStats`'s `is_available`/`rows`), and `u64` /
/// `serde_json::Value` parse anything by design; neither needs an entry.
///
/// A `TypeId` table rather than a trait bound: a `CachePayload` trait could not
/// be implemented here for the one payload private to another module
/// (`throughput::ThroughputStore`), and the bound would force an edit in its
/// home. The table keeps the guard beside the constants it protects.
fn expected_cache_file<T: 'static>() -> Option<&'static str> {
    match TypeId::of::<T>() {
        t if t == TypeId::of::<crate::usage::UsageInfo>() => Some(USAGE_CACHE_FILE),
        t if t == TypeId::of::<crate::providers::ThirdPartyStats>() => Some(THIRD_PARTY_CACHE_FILE),
        t if t == TypeId::of::<crate::usage::LimitResets>() => Some(LIMIT_RESETS_FILE),
        _ => None,
    }
}

/// Read + deserialize `<profile_dir>/<file>`. `None` on missing file or any
/// read/parse error — the caller treats both as "no cache" (matches the prior
/// per-layer loaders exactly) — and `None` again when `file` is not the cache
/// this payload type is written to, so a wrong constant cannot read an
/// all-default value out of a foreign file (see [`expected_cache_file`]).
pub(crate) fn load_profile_cache<T: DeserializeOwned + 'static>(
    name: &ProfileName,
    file: &str,
) -> Option<T> {
    if let Some(expected) = expected_cache_file::<T>()
        && expected != file
    {
        return None;
    }
    profile_cache_path(name, file).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<T>(&text).ok()
    })
}

/// Atomically write `value` to `<profile_dir>/<file>`, admitting `name` only
/// while the WRITING LEG's harness roster holds it. Failures are swallowed
/// (cache is best-effort): a missing parent is created at 0o700, the file at
/// 0o600, via a tmp + rename so a torn write reads as no cache rather than a
/// parse failure.
///
/// The harness gate keeps one harness's body out of the other's profile dir:
/// both harnesses resolve `<profile_dir>` the same way, keyed by name, so a
/// claude fetch still in flight across a delete-and-recreate of the name as a
/// codex profile would otherwise land a claude-shaped `usage_cache.json` where
/// the codex surfaces publish it as a reading. Admitting either roster can
/// never catch that — the name IS an account, just not the writing leg's.
///
/// Skips names the leg's on-disk record no longer carries: the fetch legs hold
/// a stale in-memory config for up to a tick, and the parent creation above
/// would re-create a deleted profile's directory. Fail-closed — an unreadable
/// record skips the write too. The tick-driven callers (usage fetch,
/// scheduler) retry on the next tick; one-shot writers degrade to their
/// documented safe answers instead (a lost touch receipt yields the raw
/// mtime, an unseeded anchor reads as unanchored). Cost: one roster read (a
/// TOML parse) per write — the record is small, and the claude read already
/// backs the oauth adopt gate and the daemon's switch-target existence check.
pub(crate) fn write_profile_cache_for<T: Serialize>(
    harness: Harness,
    name: &ProfileName,
    file: &str,
    value: &T,
) {
    let _ = write_profile_cache_reported_for(harness, name, file, value);
}

/// [`write_profile_cache_for`] that reports whether the file write actually
/// landed, so a caller whose cache is load-bearing (the identity anchor) can
/// log a failure instead of taking the default silent swallow. `false` covers
/// every skip (off-roster, unresolvable path, serialization) and a failed write
/// alike.
pub(crate) fn write_profile_cache_reported_for<T: Serialize>(
    harness: Harness,
    name: &ProfileName,
    file: &str,
    value: &T,
) -> bool {
    if !roster_holds(harness, name) {
        return false;
    }
    write_cache_file(name, file, value)
}

/// Whether `harness`'s own roster holds `name`: `profiles.toml` for claude
/// ([`crate::profile::is_configured`]), `codex-profiles.toml` for codex.
fn roster_holds(harness: Harness, name: &ProfileName) -> bool {
    match harness {
        Harness::Claude => claude_roster_holds(name),
        Harness::Codex => codex_roster_holds(name),
    }
}

fn claude_roster_holds(name: &ProfileName) -> bool {
    crate::profile::is_configured(name).unwrap_or(false)
}

fn codex_roster_holds(name: &ProfileName) -> bool {
    crate::codex_profiles::CodexState::load().is_ok_and(|s| s.holds(name.as_str()))
}

fn write_cache_file<T: Serialize>(name: &ProfileName, file: &str, value: &T) -> bool {
    let Some(path) = profile_cache_path(name, file) else {
        return false;
    };
    let Ok(json) = serde_json::to_string(value) else {
        return false;
    };
    crate::profile::atomic_write_600(&path, json.as_bytes()).is_ok()
}

/// Test-only either-roster writer: a fixture poses a cache under whichever
/// roster holds the name without spelling the leg it stands in for. Production
/// legs go through [`write_profile_cache_for`], which admits only their own
/// harness, so this permissive form is unreachable from a non-test build.
#[cfg(test)]
pub(crate) fn write_profile_cache<T: Serialize>(name: &ProfileName, file: &str, value: &T) {
    let _ = write_profile_cache_reported(name, file, value);
}

#[cfg(test)]
fn write_profile_cache_reported<T: Serialize>(name: &ProfileName, file: &str, value: &T) -> bool {
    if !(claude_roster_holds(name) || codex_roster_holds(name)) {
        return false;
    }
    write_cache_file(name, file, value)
}

/// Delete `<profile_dir>/<file>`. Best-effort: an already-absent file is the
/// intended post-state, and every other removal error is logged (never surfaced
/// as a failure) — a stale cache is worse than the log line that names it.
pub(crate) fn remove_profile_cache(name: &ProfileName, file: &str) {
    let Some(path) = profile_cache_path(name, file) else {
        return;
    };
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        crate::logline::logline!("clauth: {name}: could not remove its {file} cache: {e}");
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

#[cfg(test)]
#[path = "../tests/inline/profile_cache.rs"]
mod tests;
