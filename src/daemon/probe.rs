//! Daemon presence + the single-fetcher lease (dual-scheduler dedup, #27).
//!
//! Three `~/.clauth` flock files, peers in the same dir:
//!   * `clauthd.lock` — the daemon singleton + **presence beacon**. Held for
//!     life by the running daemon; a display-only try-lock tells the TUI header
//!     whether a daemon is up (an advisory lock auto-releases on process death,
//!     so a dead daemon reads as absent on the next probe). Paired with
//!     `status.json`'s freshness it drives the `[ daemon ]` header chip
//!     ([`daemon_health`]). [`singleton_held`] reads the same lock as a decision
//!     for `clauth daemon --status`, where not knowing has to be an error rather
//!     than a dim chip.
//!   * `clauthd-standby.lock` — the **standby slot** ([`StandbySlot`], #57). One
//!     waiter may park on the singleton lock; every later instance is
//!     [`Claim::Redundant`] and exits, so a spawner that fires repeatedly can no
//!     longer pile up parked daemons.
//!   * `usage-fetch.lock` — the **single-fetcher lease** ([`FetchLease`]).
//!     Exactly one instance (daemon OR a TUI) holds it at a time and does all
//!     usage fetching / rotation / switch decisions while it does; every other
//!     instance hydrates from the shared disk cache. First-come, held while the
//!     holder's ticks keep completing, and never taken from a live holder, so
//!     the switch-decider never thrashes between processes. Released on process
//!     exit, or by the holder's own lease watchdog once no tick has completed
//!     for [`LEASE_STALE_BOUND`].

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Stdio;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::logline::logline;
use crate::plugin_host::ExeChange;
use crate::profile::clauth_dir;

/// How stale `status.json` may be before the `[ daemon ]` header chip flips
/// green→amber.
/// The daemon stamps it every ~1s loop tick, but a single tick can legitimately
/// block on the keychain shell-outs: a queued switch's install gate can land a
/// rotation, whose tail makes up to four `security` calls (the foreign gate's
/// item read, then the mirror's read, write and read-back verify) at 10 s
/// each. The landing's `runtime::KEYCHAIN_MIRROR_BUDGET` arm adopts the 20 s
/// window the tick itself arms (`lock::SharedSubprocessBudget::arm_clamped`),
/// which the switch's own shell-outs and both drains' flock waits share too,
/// so a tick's shell-outs and flock waits stop at 20 s in aggregate. The
/// window sits one margin ABOVE [`WATCHDOG_DEADLINE`](super::WATCHDOG_DEADLINE),
/// so a tick inside that bound reads green throughout and amber means "no tick
/// has completed within what the watchdog tolerates" — wedging, pre-abort.
///
/// Neither bound covers a switch tick whose install gate also makes a token
/// call: `oauth::TOKEN_HTTP_DEADLINES` (19 s) runs outside the tick's window,
/// so such a tick is capped at 20 + 19 = 39 s, still past both this window and
/// the 30 s watchdog, and reads amber while it runs.
const DAEMON_STALE_MS: u64 = super::WATCHDOG_DEADLINE.as_millis() as u64 + 5_000;

const _: () = assert!(
    DAEMON_STALE_MS > super::WATCHDOG_DEADLINE.as_millis() as u64,
    "the staleness window must sit strictly above the watchdog deadline: a tick \
     whose shell-outs and flock waits spend the tick's whole 20 s window must read \
     green, never amber (a token call on top of it is not covered)"
);

/// The `[ daemon ]` header chip's three display states, derived from the daemon
/// singleton flock (presence) + the `generated_at` stamp inside `status.json`
/// (health — the daemon's own write time, not the file's mtime). Nothing here
/// gates fetching — that is [`FetchLease`] — but `clauth daemon --status` does
/// answer a caller's spawn-or-not off it, and the probe TAKES the flock it
/// tests, so a concurrent claim must survive that (see [`CLAIM_ATTEMPTS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonHealth {
    /// No daemon: `clauthd.lock` is free (never started, or the holder died).
    /// The chip is faint; the TUI self-fetches under its own lease.
    Absent,
    /// A daemon holds the lock but its feed is stale/unwritten — wedging,
    /// pre-abort, or just-booted before the first `status.json` write. Amber.
    Stale,
    /// A daemon is up and its feed is fresh. Green.
    Fresh,
}

/// Probe the daemon's presence + health for the header chip. Best-effort: any
/// error that hides whether a daemon is up reads as [`DaemonHealth::Absent`]
/// (the chip simply dims — never a false "daemon up"). Never CREATES the
/// lock file: a missing file means no daemon has ever started here.
pub(crate) fn daemon_health() -> DaemonHealth {
    if !daemon_lock_held() {
        return DaemonHealth::Absent;
    }
    // Present. A missing/unreadable feed = booted-but-not-yet-published → amber.
    let body = read_feed();
    if body
        .as_deref()
        .is_some_and(|body| status_is_fresh(body, crate::usage::now_ms()))
    {
        DaemonHealth::Fresh
    } else {
        DaemonHealth::Stale
    }
}

/// Whether a daemon holds the singleton lock, probed the way [`daemon_health`]
/// probes it: an unreadable or free lock reads `false` — never a false "daemon
/// up" — and the probe's own try-lock drops at once. Never creates the file: a
/// missing one means no daemon has ever started here.
fn daemon_lock_held() -> bool {
    let Ok(dir) = clauth_dir() else {
        return false;
    };
    let Ok(lock_file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(super::LOCK_FILE))
    else {
        return false;
    };
    match lock_file.try_lock() {
        // We took it → nobody holds it. Dropping releases it.
        Ok(()) => false,
        // Held → a daemon is present.
        Err(std::fs::TryLockError::WouldBlock) => true,
        // Can't tell (io error): read absent rather than assert a daemon.
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// One read of the daemon's published `status.json` body, `None` when it is
/// absent or unreadable. Every feed reader in this module goes through here, so
/// the one-read pin counts them all.
fn read_feed() -> Option<String> {
    #[cfg(test)]
    FEED_READS.fetch_add(1, Ordering::Relaxed);
    let dir = clauth_dir().ok()?;
    std::fs::read_to_string(dir.join(super::STATUS_FILE)).ok()
}

/// How many `status.json` reads this process has made through [`read_feed`]
/// (test builds only), so a pin can count a probe tick's reads.
#[cfg(test)]
static FEED_READS: AtomicUsize = AtomicUsize::new(0);

/// Start counting feed reads from zero for the next test. The counter is
/// process-global: reset and read it under a `HomeSandbox`, which serializes
/// every sandboxed test.
#[cfg(test)]
pub(crate) fn reset_feed_reads() {
    FEED_READS.store(0, Ordering::Relaxed);
}

/// The feed reads counted since the last reset (test builds only).
#[cfg(test)]
pub(crate) fn feed_reads() -> usize {
    FEED_READS.load(Ordering::Relaxed)
}

/// Pure freshness test: `body`'s `generated_at` stamp is within [`DAEMON_STALE_MS`]
/// of `now_ms`. An unparseable body or a missing/malformed stamp reads as stale —
/// never render a feed we can't read as fresh. A stamp in the FUTURE counts as
/// fresh (clock skew must not flap the chip).
pub(crate) fn status_is_fresh(body: &str, now_ms: u64) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|v| status_is_fresh_value(&v, now_ms))
}

/// [`status_is_fresh`] over an already-parsed feed, so a probe that reads the
/// feed once also parses it once.
fn status_is_fresh_value(v: &serde_json::Value, now_ms: u64) -> bool {
    stamp_age_ms(v, now_ms).is_some_and(|age| age <= DAEMON_STALE_MS)
}

/// The feed's `generated_at` age in ms, `None` when the stamp is missing or
/// malformed (a stamp in the future reads as age 0).
fn stamp_age_ms(v: &serde_json::Value, now_ms: u64) -> Option<u64> {
    let stamp = v.get("generated_at").and_then(|g| g.as_str())?;
    let secs = crate::usage::iso_to_epoch_secs(stamp)?;
    if secs < 0 {
        return None;
    }
    Some(now_ms.saturating_sub((secs as u64).saturating_mul(1000)))
}

/// The managed-gateway slot for the Services tab's `shunt` row: with a fresh
/// daemon, the `gateway` object the daemon published in `status.json` (the
/// supervisor's live read); otherwise the record-only verdict
/// ([`super::gateway::unsupervised_slot`]), which reads `unobserved` for a
/// record the gateway would run on. The `bool` is `true` only when the slot
/// came from a fresh daemon's feed AND its `gateway` object parsed — the caller
/// names the daemon as what runs the gateway whenever it did not.
///
/// `health` is the caller's already-probed [`daemon_health`] verdict (the
/// per-tick reading the header holds), so this reader never takes the flock a
/// second time or re-reads `status.json` just to decide freshness. A fresh
/// daemon whose feed cannot be read, whose `gateway` key is absent (an older
/// daemon), or whose `gateway` object this binary cannot parse (a state word or
/// field shape from a newer daemon) all fall back to the record verdict,
/// reported as not-from-daemon, rather than guessing a state.
pub(crate) fn gateway_slot(health: DaemonHealth) -> (super::gateway::GatewaySlot, bool) {
    if health != DaemonHealth::Fresh {
        return (super::gateway::unsupervised_slot(), false);
    }
    // Parsed straight from the text: the struct parse skips every other key, so
    // a caller polling every tick builds no tree of the whole feed.
    match read_feed() {
        Some(body) => gateway_slot_of(serde_json::from_str::<GatewayFeed>(&body)),
        None => (super::gateway::unsupervised_slot(), false),
    }
}

/// The `gateway` key alone, so one unparseable key never discards a fresh
/// daemon's slot.
#[derive(serde::Deserialize)]
struct GatewayFeed {
    #[serde(default)]
    gateway: Option<super::gateway::GatewaySlot>,
}

/// The slot a parsed [`GatewayFeed`] names, else the record verdict as
/// not-from-daemon.
fn gateway_slot_of(
    feed: Result<GatewayFeed, serde_json::Error>,
) -> (super::gateway::GatewaySlot, bool) {
    match feed.ok().and_then(|feed| feed.gateway) {
        Some(slot) => (slot, true),
        None => (super::gateway::unsupervised_slot(), false),
    }
}

/// [`gateway_slot`]'s mapping over a feed [`probe_daemon`] already parsed.
fn gateway_slot_value(v: &serde_json::Value) -> (super::gateway::GatewaySlot, bool) {
    gateway_slot_of(<GatewayFeed as serde::Deserialize>::deserialize(v))
}

/// How a daemon's own executable changed since it started, so its plugin heal
/// stands down until it restarts: `binary_removed: true` reads as removed, else
/// `binary_replaced: true` as replaced. Absent keys (an older daemon) or a feed
/// this binary cannot parse read `None` — the two binary keys alone, as
/// [`gateway_slot_value`] parses its own.
fn binary_change_value(v: &serde_json::Value) -> Option<ExeChange> {
    #[derive(serde::Deserialize)]
    struct BinaryFeed {
        #[serde(default)]
        binary_replaced: bool,
        #[serde(default)]
        binary_removed: bool,
    }
    let feed = <BinaryFeed as serde::Deserialize>::deserialize(v).ok()?;
    if feed.binary_removed {
        Some(ExeChange::Removed)
    } else if feed.binary_replaced {
        Some(ExeChange::Replaced)
    } else {
        None
    }
}

/// One probe's whole take: the lock verdict plus ONE read and parse of the
/// feed, shared by the consumers that would otherwise each re-read the file
/// (the freshness verdict, [`gateway_slot_value`]'s slot,
/// [`binary_change_value`]'s flags). The TUI's per-second tick and the tab
/// re-probe drive this instead of the three readers.
pub(crate) struct DaemonProbe {
    /// The header chip's verdict.
    pub(crate) health: DaemonHealth,
    /// The gateway slot, as [`gateway_slot`] reports it.
    pub(crate) gateway: (super::gateway::GatewaySlot, bool),
    /// The binary-change flags, as [`binary_change_value`] reads them.
    pub(crate) binary_change: Option<ExeChange>,
}

/// Run one probe: the lock verdict, then one read and one parse shared by every
/// consumer. A non-fresh verdict short-circuits the two feed consumers to their
/// no-fresh-daemon answers, exactly as [`gateway_slot`] does when its health
/// gate fails.
pub(crate) fn probe_daemon() -> DaemonProbe {
    if !daemon_lock_held() {
        return DaemonProbe {
            health: DaemonHealth::Absent,
            gateway: (super::gateway::unsupervised_slot(), false),
            binary_change: None,
        };
    }
    let body = read_feed();
    let value = body
        .as_deref()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok());
    let health = match value.as_ref() {
        Some(v) if status_is_fresh_value(v, crate::usage::now_ms()) => DaemonHealth::Fresh,
        _ => DaemonHealth::Stale,
    };
    let (gateway, binary_change) = match value.as_ref() {
        Some(v) if health == DaemonHealth::Fresh => (gateway_slot_value(v), binary_change_value(v)),
        _ => ((super::gateway::unsupervised_slot(), false), None),
    };
    DaemonProbe {
        health,
        gateway,
        binary_change,
    }
}

/// The managed proxies' slots for `clauth proxy list` (and the P8 TUI): with a
/// fresh daemon, the `proxies` array the daemon published in `status.json`
/// (the supervisors' live reads); otherwise the record-only entries
/// ([`super::proxies::entries`] over no live slots). A fresh daemon whose feed
/// cannot be read, whose `proxies` key is absent (an older daemon), or whose
/// array this binary cannot parse falls back to the record-only entries rather
/// than guessing a state — the [`gateway_slot`] discipline, for the array.
pub(crate) fn proxy_slots(health: DaemonHealth) -> Vec<super::proxies::ProxySlot> {
    if health != DaemonHealth::Fresh {
        return super::proxies::entries(None);
    }
    let Some(body) = read_feed() else {
        return super::proxies::entries(None);
    };
    // Parse only the `proxies` key: one unrelated profile entry that this
    // binary cannot read must not discard a fresh daemon's slots.
    #[derive(serde::Deserialize)]
    struct ProxiesFeed {
        #[serde(default)]
        proxies: Option<Vec<super::proxies::ProxySlot>>,
    }
    let Ok(feed) = serde_json::from_str::<ProxiesFeed>(&body) else {
        return super::proxies::entries(None);
    };
    match feed.proxies {
        Some(slots) => slots,
        None => super::proxies::entries(None),
    }
}

/// What a starting `clauth daemon` is allowed to become (#57).
///
/// Standby exists because a supervisor's instance must be able to take over
/// from a manually-run one without a clean exit it would never be restarted
/// from (launchd `KeepAlive{SuccessfulExit=false}`). It is capped at ONE waiter:
/// before the cap, every `clauth daemon` a repeat-firing spawner started parked
/// forever, which is how one box collected two dozen of them.
#[derive(Debug)]
pub(crate) enum Claim {
    /// Took `clauthd.lock`. Run the scheduler; hold the guard for life.
    Active(DaemonLock),
    /// Another daemon holds the lock and this process took the one standby
    /// slot. [`StandbySlot::promote`] blocks until the holder exits.
    Standby(StandbySlot),
    /// A daemon is up and the standby slot is taken (or standby was declined):
    /// there is nothing left for this process to do.
    Redundant,
}

/// The held singleton lock. Kept for the process lifetime; the flock releases
/// when the process exits, so a crashed daemon frees it with no pidfile cleanup.
#[derive(Debug)]
pub(crate) struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    /// Take ownership of the just-locked file and stamp the holder's pid into
    /// the unlocked [`PID_FILE`] sidecar. The pid is informational (it answers
    /// "which of these is the live one?" in a `ps` dump); presence is proven by
    /// the flock alone, so a stale pid left by a dead holder can never read as a
    /// running daemon.
    fn active(file: File) -> Self {
        let _ = stamp_pid();
        Self { _file: file }
    }
}

/// The one standby slot: the (still unlocked) singleton handle this process
/// will block on, plus the `clauthd-standby.lock` flock that keeps every later
/// instance out of the queue.
#[derive(Debug)]
pub(crate) struct StandbySlot {
    active: File,
    slot: File,
}

impl StandbySlot {
    /// Block until the running daemon exits, then take over. Both flocks are
    /// held across the wait, so the slot reopens for the next arrival only once
    /// this process is the daemon — never mid-promotion.
    pub(crate) fn promote(self) -> Result<DaemonLock> {
        self.active
            .lock()
            .context("failed to acquire the clauth daemon lock")?;
        drop(self.slot);
        Ok(DaemonLock::active(self.active))
    }
}

/// How many times a non-[`Claim::Active`] outcome is re-tried before it stands.
/// **Every presence probe TAKES the flock it tests** — [`daemon_health`] (TUI
/// header, 1 Hz), [`singleton_held`] (every caller deciding on a daemon's
/// presence, `clauth daemon --status` at whatever rate a supervisor polls
/// among them) and [`standby_waiting`] (the
/// slot file) try-lock a free file and release it microseconds later — so a single
/// lost try-lock does not prove a daemon is there. A real holder keeps its lock for the process
/// lifetime, so anything that clears on the next attempt was a reader. Without
/// this, a µs-long probe could push a starting daemon into `Redundant`, and
/// under launchd `KeepAlive{SuccessfulExit=false}` that clean exit is never
/// restarted.
const CLAIM_ATTEMPTS: u32 = 3;
/// Spacing between those attempts. Two orders of magnitude above a probe's hold,
/// far below any human-visible startup delay.
const CLAIM_RETRY: Duration = Duration::from_millis(100);
// A single attempt, or a zero gap between attempts, is the pre-fix behaviour:
// the whole window collapses inside one probe's hold and a starting daemon exits
// for good. Every schedule-injecting test still passes with either, so the guard
// is here rather than in one.
const _: () = assert!(CLAIM_ATTEMPTS > 1 && !CLAIM_RETRY.is_zero());

/// How long `--replace` (and the TUI's `stop daemon`) waits for a terminated
/// holder's flock to auto-release on death before it escalates (SIGTERM →
/// SIGKILL on unix, another `/F` on Windows). A holder signalled mid token
/// spend waits that spend out ([`SPEND_DRAIN_BOUND`](super::SPEND_DRAIN_BOUND))
/// while its children stop inside `DAEMON_STOP_BUDGET` plus their answer
/// grace, so the force kill must wait past the longer of the two plus one
/// poll, or it lands between the token endpoint's answer and the save. This
/// derives that longer term (the drain) plus five seconds of headroom; the
/// children's side is pinned beside its own constants.
pub(super) const REPLACE_WAIT: Duration =
    super::SPEND_DRAIN_BOUND.saturating_add(Duration::from_secs(5));
const _: () = assert!(
    super::SPEND_DRAIN_BOUND.as_millis() >= crate::oauth::TOKEN_HTTP_DEADLINES.as_millis()
        && super::SPEND_DRAIN_BOUND.as_millis() + REPLACE_POLL.as_millis()
            < REPLACE_WAIT.as_millis(),
    "the drain must outlast one token call, and the force kill that drain by at least one poll"
);
/// How long the pass after the escalation waits for the force-killed holder's
/// flock: a dead process's flock releases on death, so this needs no token
/// save's time, only headroom over the kernel's teardown.
pub(super) const KILL_WAIT: Duration = Duration::from_secs(5);
/// Poll spacing while `--replace` waits for the freed flock. Two orders of
/// magnitude below [`REPLACE_WAIT`], well under any human-visible delay.
pub(super) const REPLACE_POLL: Duration = Duration::from_millis(50);

/// Claim a role in the daemon singleton, re-trying a lost race past a transient
/// probe hold (see [`CLAIM_ATTEMPTS`]).
pub(crate) fn claim_singleton(dir: &Path, standby: bool) -> Result<Claim> {
    claim_singleton_with(dir, standby, CLAIM_ATTEMPTS, CLAIM_RETRY)
}

/// [`claim_singleton`] with the retry schedule injected, so a test can pin the
/// probe-collision recovery without sleeping for it.
pub(crate) fn claim_singleton_with(
    dir: &Path,
    standby: bool,
    attempts: u32,
    retry: Duration,
) -> Result<Claim> {
    let mut claim = claim_once(dir, standby)?;
    for _ in 1..attempts.max(1) {
        // Only a `Redundant` is worth re-testing. A won slot is a real outcome:
        // re-taking it would leave the one standby seat free for most of the
        // window and delay a standby by a wait it has no use for.
        if matches!(claim, Claim::Active(_) | Claim::Standby(_)) {
            break;
        }
        debug_assert!(
            matches!(claim, Claim::Redundant),
            "only a Redundant reaches the retry: a won claim still owns its flocks here, because \
             assignment drops the old value AFTER claim_once has already run. The next attempt \
             would race its own slot lock and read itself as a foreign holder"
        );
        std::thread::sleep(retry);
        claim = claim_once(dir, standby)?;
    }
    Ok(claim)
}

/// Test-only: claim attempts each directory has seen, keyed by directory because
/// tests run in parallel over distinct sandboxes. Lets a test pin "the retry leg
/// did not run" by counting attempts instead of by a wall-clock bound.
#[cfg(test)]
static CLAIM_ATTEMPTS_BY_DIR: std::sync::LazyLock<
    Mutex<std::collections::HashMap<std::path::PathBuf, u64>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Claim attempts `dir` has seen so far (test builds only).
#[cfg(test)]
fn claim_attempts(dir: &Path) -> u64 {
    CLAIM_ATTEMPTS_BY_DIR
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(dir)
        .copied()
        .unwrap_or(0)
}

/// One claim attempt. `standby` false (`--no-standby`) turns a lost race straight
/// into [`Claim::Redundant`] without so much as creating the slot file.
///
/// An io error from either try-lock propagates instead of reading as "somebody
/// holds it": on a filesystem without working locks (NFS/CIFS `ENOLCK`) every
/// instance would otherwise announce a daemon that isn't there and exit 0. A
/// hard failure exits non-zero, which a supervisor retries and an operator sees.
fn claim_once(dir: &Path, standby: bool) -> Result<Claim> {
    #[cfg(test)]
    {
        *CLAIM_ATTEMPTS_BY_DIR
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(dir.to_path_buf())
            .or_insert(0) += 1;
    }
    let active = crate::profile::open_state_file(&dir.join(super::LOCK_FILE))
        .context("failed to open the clauth daemon lock file")?;
    match active.try_lock() {
        Ok(()) => return Ok(Claim::Active(DaemonLock::active(active))),
        Err(std::fs::TryLockError::WouldBlock) => {}
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).context("failed to lock the clauth daemon lock file");
        }
    }
    if !standby {
        return Ok(Claim::Redundant);
    }
    let slot = crate::profile::open_state_file(&dir.join(super::STANDBY_LOCK_FILE))
        .context("failed to open the clauth daemon standby lock file")?;
    match slot.try_lock() {
        Ok(()) => Ok(Claim::Standby(StandbySlot { active, slot })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(Claim::Redundant),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).context("failed to lock the clauth daemon standby lock file")
        }
    }
}

/// `clauth daemon --replace`: terminate the running daemon and take the
/// singleton lock, for an in-place upgrade (#57). Always yields [`Claim::Active`]
/// on success; every failure mode is an `Err`, never a silent stand-down.
///
/// Nothing running (a free or missing lock) means nothing to replace: take the
/// lock like a normal start. A running daemon whose pid can't be read (a torn or
/// in-handover [`PID_FILE`] sidecar) bails rather than signal a pid it can't
/// confirm. Otherwise it signals the holder (SIGTERM then SIGKILL on unix;
/// `taskkill /F` on both passes on Windows, where no graceful kill exists for
/// a console daemon), waits for the flock to auto-release on death (bounded),
/// escalates once, then claims. The
/// identity guard narrows the recycled-pid window [`holder_pid`] documents by
/// requiring the pid to still be a running `clauth daemon` (not merely a clauth
/// process — `clauth start`/`mcp`/`tui` share the binary name), so a stale pid
/// recycled onto an unrelated process, or onto another clauth subcommand, is
/// never signalled.
pub(crate) fn claim_by_replacing(dir: &Path) -> Result<Claim> {
    claim_by_replacing_with(dir, REPLACE_WAIT, KILL_WAIT, REPLACE_POLL)
}

/// [`claim_by_replacing`] with the wait schedule injected, so a test can pin the
/// poll loop and the escalation without sleeping the full [`REPLACE_WAIT`].
/// Always yields [`Claim::Active`] on success; every failure mode is an `Err`,
/// never a silent [`Claim::Redundant`] that would log-and-exit-0 on the caller.
pub(crate) fn claim_by_replacing_with(
    dir: &Path,
    wait: Duration,
    kill_wait: Duration,
    poll: Duration,
) -> Result<Claim> {
    claim_by_replacing_retry_with(dir, wait, kill_wait, poll, CLAIM_ATTEMPTS, CLAIM_RETRY)
}

/// [`claim_by_replacing_with`] with the retry schedule injected, so a test can
/// pin the probe-collision recovery without sleeping for it.
pub(crate) fn claim_by_replacing_retry_with(
    dir: &Path,
    wait: Duration,
    kill_wait: Duration,
    poll: Duration,
    attempts: u32,
    retry: Duration,
) -> Result<Claim> {
    // If the retry cleared the transient, try a normal claim. If a daemon wins
    // the lock in the instant between the presence check and the claim, fall
    // through and replace it rather than returning a silent `Redundant` (which
    // `serve` would log and exit 0 on, leaving the operator's upgrade un-started).
    if !held_past_probes(attempts, retry)?
        && let Claim::Active(lock) = claim_once(dir, false)?
    {
        return Ok(Claim::Active(lock));
    }
    // The flock a dying holder released is what this waits on, so the poll is
    // load-bearing: the first attempt races the death and normally still reads
    // the lock held.
    let lock = terminate_holder(wait, kill_wait, poll, |_| claim_active(dir))?;
    Ok(Claim::Active(lock))
}

/// The singleton, when this process can take it now.
fn claim_active(dir: &Path) -> Option<DaemonLock> {
    match claim_once(dir, false) {
        Ok(Claim::Active(lock)) => Some(lock),
        _ => None,
    }
}

/// What [`stop_running`] found and left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonStop {
    /// No daemon held the singleton: nothing was signalled.
    NotRunning,
    /// The daemon exited and the singleton stayed free.
    Stopped,
    /// The daemon exited and another instance held the singleton at once: a
    /// parked standby promoted, or a supervisor restarted it.
    Replaced,
}

/// The TUI's `stop daemon`: the termination `--replace` sends, with no claim
/// after it, so the box is left with no daemon rather than this process.
pub(crate) fn stop_running() -> Result<DaemonStop> {
    stop_running_with(
        REPLACE_WAIT,
        KILL_WAIT,
        REPLACE_POLL,
        CLAIM_ATTEMPTS,
        CLAIM_RETRY,
    )
}

/// [`stop_running`] with the wait and retry schedules injected, so a test can
/// pin the escalation and the probe-collision recovery without sleeping for
/// either.
pub(crate) fn stop_running_with(
    wait: Duration,
    kill_wait: Duration,
    poll: Duration,
    attempts: u32,
    retry: Duration,
) -> Result<DaemonStop> {
    if !held_past_probes(attempts, retry)? {
        return Ok(DaemonStop::NotRunning);
    }
    // Death is the event, not the free lock: a parked standby takes the flock
    // the instant it is released, so a free-lock wait would sit out both
    // passes and then SIGKILL a pid that is already gone.
    terminate_holder(wait, kill_wait, poll, |pid| {
        (!pid_is_clauth_daemon(pid)).then_some(())
    })?;
    if held_past_probes(attempts, retry)? {
        // The successor's own start reclaims whatever gateway was left.
        return Ok(DaemonStop::Replaced);
    }
    super::gateway::stop_left_behind_gateway();
    super::proxies::stop_left_behind_proxies();
    Ok(DaemonStop::Stopped)
}

/// Whether a daemon holds the singleton, re-tested past transient probe holds
/// (TUI header at 1 Hz, `clauth daemon --status`) that take the flock and
/// release it microseconds later: a real holder keeps its lock for the process
/// lifetime, so anything that clears on retry was a reader.
fn held_past_probes(attempts: u32, retry: Duration) -> Result<bool> {
    let mut held = true;
    for attempt in 0..attempts.max(1) {
        held = singleton_held()?;
        if !held {
            break;
        }
        if attempt + 1 < attempts.max(1) {
            std::thread::sleep(retry);
        }
    }
    Ok(held)
}

/// Signal the daemon the [`PID_FILE`] sidecar names until `released` answers
/// for its pid: SIGTERM (`taskkill /F` on Windows), a bounded wait of `wait`,
/// one escalation (SIGKILL, another `/F`), then a bounded wait of `kill_wait`
/// for the dead process's flock to go. Shared by `--replace` and
/// [`stop_running`], which differ only in what counts as done.
fn terminate_holder<T>(
    wait: Duration,
    kill_wait: Duration,
    poll: Duration,
    mut released: impl FnMut(u32) -> Option<T>,
) -> Result<T> {
    let Some(pid) = holder_pid() else {
        anyhow::bail!("a clauth daemon is running but its pid is unreadable; kill it manually");
    };
    if !pid_is_clauth_daemon(pid) {
        anyhow::bail!(
            "the recorded daemon pid {pid} is not a running clauth daemon (it exited during \
             handover or was recycled); re-run once it settles, or kill the daemon manually"
        );
    }
    let sent_term = terminate_pid(pid, false);
    if let Some(done) = poll_until(wait, poll, || released(pid)) {
        return Ok(done);
    }
    let sent_kill = terminate_pid(pid, true);
    if let Some(done) = poll_until(kill_wait, poll, || released(pid)) {
        return Ok(done);
    }
    if !sent_term && !sent_kill {
        anyhow::bail!(
            "could not signal the running clauth daemon (pid {pid}): no kill tool is on PATH \
             (`kill` on unix, `taskkill` on Windows); kill it manually"
        );
    }
    anyhow::bail!(
        "the running clauth daemon (pid {pid}) did not release the lock within {}s of the force \
         kill; it may be wedged uninterruptibly",
        kill_wait.as_secs()
    )
}

/// Ask `done` every `poll` until it answers, up to `wait`; `None` on timeout.
pub(super) fn poll_until<T>(
    wait: Duration,
    poll: Duration,
    mut done: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(value) = done() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}

/// Send SIGTERM (`hard` false) or SIGKILL (`hard` true) to `pid`, returning
/// whether the signal command actually ran (`false` = it could not even be
/// spawned, e.g. `kill` off PATH — which the caller distinguishes from "signalled
/// but the process survived"). Shelling out spends no `unsafe` here
/// (`libc::kill` would need it, and `signal_hook` only *receives*), matching
/// the `/usr/bin/security` shell-out pattern already in the crate. A non-zero exit
/// (a dead pid's `ESRCH`) still counts as run: the caller polls the flock either
/// way. Long-form flags so the call site documents itself.
#[cfg(unix)]
pub(super) fn terminate_pid(pid: u32, hard: bool) -> bool {
    let signal = if hard { "KILL" } else { "TERM" };
    let mut cmd = std::process::Command::new("kill");
    cmd.args(["-s", signal, &pid.to_string()]);
    // A soft-pass refusal (a dead pid's ESRCH) is expected noise: silence it.
    // The hard pass stays loud — its failure is the diagnosis the generic
    // wedged-process bail lacks.
    if !hard {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    cmd.status().is_ok()
}

#[cfg(windows)]
pub(super) fn terminate_pid(pid: u32, hard: bool) -> bool {
    // A console daemon has no window to accept the graceful WM_CLOSE, so a
    // soft taskkill can never work here: every pass is a force kill. The
    // first (soft) pass is the expected-success path and stays silenced; the
    // hard pass only runs when that first kill failed, so it keeps stderr as
    // the wedged-process diagnosis. stdout never carries anything the caller
    // reads (taskkill prints its SUCCESS line there).
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/F"]);
    cmd.stdout(Stdio::null());
    if !hard {
        cmd.stderr(Stdio::null());
    }
    cmd.status().is_ok()
}

/// Confirm `pid` is still a running `clauth daemon` before `--replace` signals
/// it, narrowing the recycled-pid window [`holder_pid`] documents. The check is
/// on the daemon ROLE, not the binary name: `clauth start` (resident around a
/// live Claude Code session), `clauth mcp`, and `clauth tui` all share
/// `comm == "clauth"`, so a name-only guard would let `--replace` force-kill
/// one of them if the stale pid recycled onto it. Requiring argv `clauth daemon …`
/// excludes every other subcommand and every non-clauth process. A pid that has
/// exited (the in-handover window: recorded pid gone, successor holds the lock)
/// or that can't be verified reads as false, so `--replace` bails and the
/// operator re-runs against the settled successor rather than signalling blind.
#[cfg(target_os = "linux")]
fn pid_is_clauth_daemon(pid: u32) -> bool {
    // `/proc/<pid>/cmdline` is NUL-separated argv; a gone pid fails the read.
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    cmdline_is_daemon(
        raw.split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| a.to_vec()),
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn pid_is_clauth_daemon(pid: u32) -> bool {
    // macOS/BSD: `ps -p <pid> -o command=` prints the full argv, space-joined. A
    // missing pid prints nothing; a failed `ps` reads false so a broken probe
    // never green-lights a signal.
    let Ok(out) = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let cmd = String::from_utf8_lossy(&out.stdout);
    cmdline_is_daemon(cmd.split_whitespace().map(|t| t.as_bytes().to_vec()))
}

#[cfg(windows)]
fn pid_is_clauth_daemon(pid: u32) -> bool {
    // `tasklist` gives the image name but not the full argv, so this confirms
    // only that the pid is a clauth image (rejecting a recycle onto an unrelated
    // process). It can't tell the daemon from another clauth subcommand here — a
    // documented residual on Windows, where the recycle window is a handful of
    // instructions wide. A failed probe reads false: never signal unverified.
    let Ok(out) = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
    else {
        return false;
    };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .to_ascii_lowercase()
            .contains("clauth")
}

/// Shared argv test for the unix identity guards: argv[0]'s basename is `clauth`
/// and argv[1] is `daemon`. Takes owned tokens so both the `/proc` (NUL-split
/// bytes) and `ps` (whitespace-split str) callers feed one predicate.
#[cfg(unix)]
fn cmdline_is_daemon(mut args: impl Iterator<Item = Vec<u8>>) -> bool {
    let argv0_is_clauth = args
        .next()
        .and_then(|a| String::from_utf8(a).ok())
        .map(|a| a.rsplit('/').next().unwrap_or(&a) == "clauth")
        .unwrap_or(false);
    let argv1_is_daemon = args
        .next()
        .and_then(|a| String::from_utf8(a).ok())
        .is_some_and(|a| a == "daemon");
    argv0_is_clauth && argv1_is_daemon
}

/// Overwrite the unlocked [`PID_FILE`] sidecar with the current pid. The pid
/// deliberately does NOT live in `clauthd.lock`: Windows locks are mandatory
/// (`LockFileEx`), so a `--status` reader in another process cannot read bytes
/// the daemon holds under its exclusive lock — the sidecar is the one place
/// every platform can read. Only ever one writer (the single Active daemon),
/// but readers race it, so truncate-first: the trailing newline is the commit
/// marker [`holder_pid`] requires — a reader that catches the write half-done
/// sees no newline and reports nothing rather than parsing a truncated pid into
/// a live unrelated process. Best-effort: a daemon that can't write its own pid
/// still runs (the sidecar then reads as "unknown").
fn stamp_pid() -> std::io::Result<()> {
    let dir = clauth_dir().map_err(std::io::Error::other)?;
    let mut file = crate::profile::open_state_file(&dir.join(super::PID_FILE))?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()
}

/// The pid the running daemon stamped into the [`PID_FILE`] sidecar, when one is
/// fully written. Informational only — its callers reach it past a held flock
/// ([`singleton_held`] true, or [`daemon_health`] not absent), so
/// presence is proven by the flock either way and a pid left behind by a dead
/// daemon is never read as one being up.
///
/// One residual, accepted: the newline sentinel rules out a TORN stamp, not a
/// complete stale one. Between a successor winning the flock and its
/// [`stamp_pid`] reaching `set_len(0)` — a handful of instructions — the dead
/// predecessor's pid is still whole and readable, so `--status` can print a pid
/// the OS has since recycled onto something unrelated. The sidecar is never read
/// unless the flock is held, so a lingering pid from a fully
/// dead daemon is out of reach; only this in-handover window can surface a stale
/// one, and truncate-first keeps it to a handful of instructions.
pub(crate) fn holder_pid() -> Option<u32> {
    let dir = clauth_dir().ok()?;
    let body = std::fs::read_to_string(dir.join(super::PID_FILE)).ok()?;
    // No terminating newline → the stamp is torn or absent. Never guess.
    body.strip_suffix('\n')?.trim().parse().ok()
}

/// Whether a daemon holds the singleton lock, as a DECISION rather than a
/// display: every way of not knowing is an error instead of a "no". Filesystems
/// without working locks (NFS/CIFS `ENOLCK`) are the case that matters — there
/// [`daemon_health`] deliberately reads `Absent`, and a `--status || spawn`
/// supervisor built on that answer respawns forever with the cause visible only
/// in `daemon.log`. Never creates the file: a missing one is a real "no daemon
/// has ever started here".
pub(crate) fn singleton_held() -> Result<bool> {
    let dir = clauth_dir()?;
    let path = dir.join(super::LOCK_FILE);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "failed to open the clauth daemon lock file {}",
                    path.display()
                )
            });
        }
    };
    match file.try_lock() {
        // We took it → nobody holds it. Dropping releases it.
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).context("failed to test the clauth daemon lock file")
        }
    }
}

/// True when a second `clauth daemon` is parked in the standby slot. Never
/// creates the file: a missing one means nobody has ever stood by here.
pub(crate) fn standby_waiting() -> bool {
    let Ok(dir) = clauth_dir() else {
        return false;
    };
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(super::STANDBY_LOCK_FILE))
    else {
        return false;
    };
    // Took it → free → nobody is standing by. Dropping releases it.
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// The stand-down / acquired lines, each logged only on its own edge of
/// [`FetchLease::stood_down`].
const STOOD_DOWN_LINE: &str = concat!(
    "clauth: another instance holds the usage-fetch lease: standing down ",
    "(rendering from the shared cache)"
);
const ACQUIRED_LINE: &str = "clauth: acquired the usage-fetch lease: fetching";

/// Log a stand-down / lease-acquired transition. Either the TUI or the daemon
/// can stand down (whichever didn't win the lease). [`logline!`] routes by
/// PROCESS, never by the calling thread: the daemon's line (stamping on) to
/// `daemon.log`, an interactive TUI's (stderr still a terminal) to
/// `~/.clauth/clauth.log` — so the line a TUI raises on its UI thread from
/// `App::finish_bootstrap` is recorded without ever painting over the accounts
/// pane.
fn standdown_transition_log(msg: &str) {
    logline!("{msg}");
}

/// The single-fetcher lease over `usage-fetch.lock` (#27). Once acquired the
/// [`File`] is retained while this instance's ticks keep completing, so the
/// flock is NOT re-acquired per tick, which would bounce the switch-decider
/// between processes (the #27 switch thrash). It is released on exit, or by
/// [`Self::release_if_stale`] once no tick has completed for
/// [`LEASE_STALE_BOUND`], so a wedged tick thread frees it for a standby.
///
/// The inner [`Mutex`] is a pure leaf: locked only inside
/// [`acquire_ticket`](Self::acquire_ticket) and [`Self::release_if_stale`],
/// and released before either returns, so it carries no lock rank. The flock
/// itself sits outside the compile-time rank system and must be taken
/// OUTERMOST of the tick's ranked locks (`acquire_ticket` runs at the very top
/// of the tick).
#[derive(Default, Debug)]
pub(crate) struct FetchLease {
    held: Mutex<Option<File>>,
    /// Whether this instance does not fetch: the last
    /// [`acquire`](Self::acquire) stood down, or the watchdog released the
    /// lease since. The one record of who fetches: a display surface reads it
    /// to tell a row of its own due schedule from a mirror's cache-mtime
    /// estimate, and an acquire's two edges on it are the only places the
    /// stand-down / acquired lines are logged, so a UI-thread acquire and a
    /// tick acquire can never disagree about either.
    stood_down: AtomicBool,
    /// Set when THIS instance takes the lease, wherever the take happened (a
    /// TUI's bootstrap acquires on the UI thread before the first tick), and
    /// taken once by [`Self::take_new_hold`].
    new_hold: AtomicBool,
    /// Moved by every watchdog release, before the flock drops. A tick holds
    /// the generation it acquired under ([`LeaseTicket`]), so one that resumes
    /// after losing the lease sees it moved and applies nothing.
    generation: AtomicU64,
    /// Epoch ms of the last completed tick, or of the take when no tick has
    /// completed since: what [`Self::release_if_stale`] ages.
    last_tick_ms: AtomicU64,
}

/// A fetch tick's hold on the lease: the generation the lease was held under
/// when the tick began. Copy, so every leg and worker of the tick carries it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LeaseTicket<'a> {
    lease: &'a FetchLease,
    generation: u64,
}

impl LeaseTicket<'_> {
    /// Whether the lease is still held under this ticket: no watchdog release
    /// has moved the generation since the tick acquired it.
    pub(crate) fn is_current(self) -> bool {
        self.lease.generation() == self.generation
    }

    /// The generation the ticket was taken under.
    pub(crate) fn generation(self) -> u64 {
        self.generation
    }
}

impl FetchLease {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Become — or confirm we already are — the single fetcher. Returns `true`
    /// when THIS instance holds the lease (fetch this tick), `false` when another
    /// instance holds it or the lock is unreadable (stand down and hydrate from
    /// the shared cache). Idempotent once held: a held lease short-circuits to
    /// `true`, so the flock is never re-taken. Every step of the attempt past
    /// that short-circuit — an unresolvable `~/.clauth`, an unopenable lock file,
    /// either try-lock error — yields no lease, so all of them stand down
    /// through the one tail below: an io error is never a licence to dup-fetch.
    ///
    /// Each transition line is logged on [`Self::stood_down`]'s own edge: the
    /// callers include a TUI's `finish_bootstrap` on the UI thread and the
    /// scheduler's tick, so a caller-side flag would miss the other caller's
    /// stand-down and log it never or twice. The edge is decided under the
    /// `held` guard — two concurrent callers still split one transition between
    /// them — and the line is logged after the guard drops, so two callers'
    /// lines can reach the log in either order: the log's order across threads
    /// is not the edge order, and `stood_down` alone says which edge came last.
    pub(crate) fn acquire(&self) -> bool {
        self.acquire_ticket().is_some()
    }

    /// [`Self::acquire`] for a fetch tick: the ticket carries the generation
    /// read under the same `held` guard a watchdog release moves it under, so
    /// no release can fall between the hold and the read. A take stamps the
    /// tick clock, so a holder that has completed no tick yet ages from its
    /// take.
    pub(crate) fn acquire_ticket(&self) -> Option<LeaseTicket<'_>> {
        let mut held = match self.held.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if held.is_some() {
            return Some(self.ticket());
        }
        let taken = self.try_take_lease();
        let fetched = taken.is_some();
        if let Some(file) = taken {
            *held = Some(file);
            self.last_tick_ms
                .store(crate::usage::now_ms(), Ordering::Relaxed);
            self.new_hold.store(true, Ordering::Relaxed);
        }
        let ticket = fetched.then(|| self.ticket());
        let transition = if fetched {
            self.mark_acquired()
        } else {
            self.mark_stood_down()
        };
        drop(held);
        if let Some(line) = transition {
            standdown_transition_log(line);
        }
        ticket
    }

    fn ticket(&self) -> LeaseTicket<'_> {
        LeaseTicket {
            lease: self,
            generation: self.generation(),
        }
    }

    /// A ticket re-derived under a generation captured at spawn: what a spawned
    /// worker carries, so a worker that outlives its tick still checks against
    /// the generation the tick acquired under.
    pub(crate) fn ticket_at(&self, generation: u64) -> LeaseTicket<'_> {
        LeaseTicket {
            lease: self,
            generation,
        }
    }

    /// Take the lease file, or `None` when the attempt cannot yield one. Every
    /// failure collapses here, so no failure can skip the caller's tail and
    /// answer `false` without recording the stand-down.
    fn try_take_lease(&self) -> Option<File> {
        let dir = clauth_dir().ok()?;
        // The lease file lives in `~/.clauth`; ensure the dir exists (best-effort,
        // 0o700) so a first-run TUI with no daemon can still take the lease.
        // `serve()` already creates it for the daemon; a TUI normally reaches here
        // with the dir already present (profiles live in it).
        let _ = crate::profile::mkdir_700(&dir);
        let file = crate::profile::open_state_file(&dir.join(super::FETCH_LOCK_FILE)).ok()?;
        match file.try_lock() {
            Ok(()) => Some(file),
            Err(std::fs::TryLockError::WouldBlock | std::fs::TryLockError::Error(_)) => None,
        }
    }

    /// Record a stand-down and return the line to log when this call is the
    /// false→true edge, so a tick that keeps losing the lease says so once.
    /// The caller logs it after the `held` guard drops.
    fn mark_stood_down(&self) -> Option<&'static str> {
        (!self.stood_down.swap(true, Ordering::Relaxed)).then_some(STOOD_DOWN_LINE)
    }

    /// Record an acquire and return the line to log when this call is the
    /// true→false edge. The caller logs it after the `held` guard drops.
    fn mark_acquired(&self) -> Option<&'static str> {
        self.stood_down
            .swap(false, Ordering::Relaxed)
            .then_some(ACQUIRED_LINE)
    }

    /// Whether this instance runs the usage fetches itself. `false` means it
    /// mirrors another process's disk, so the deadline it publishes is its own
    /// cache-mtime estimate rather than a fetch it is about to run. A lease not
    /// yet tried reads `true`: an instance that has not stood down is the one
    /// about to fetch.
    pub(crate) fn fetches_here(&self) -> bool {
        !self.stood_down.load(Ordering::Relaxed)
    }

    /// Whether this instance took the lease since the last call: `true` once
    /// per take, so the first tick under a lease taken over from another
    /// instance can read what the old holder recorded.
    pub(crate) fn take_new_hold(&self) -> bool {
        self.new_hold.swap(false, Ordering::Relaxed)
    }

    /// Record a completed tick at `now_ms`, the clock
    /// [`Self::release_if_stale`] ages.
    pub(crate) fn stamp_tick(&self, now_ms: u64) {
        self.last_tick_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Give the lease up when this instance holds it and its last completed
    /// tick is more than `bound` older than `now_ms`. The generation moves
    /// BEFORE the flock drops, so a tick that reads the old generation inside a
    /// state-flock hold proves no standby has taken the lease yet. The lock
    /// file is never unlinked: a racer would split across two inodes. Returns
    /// whether it released.
    pub(crate) fn release_if_stale(&self, now_ms: u64, bound: Duration) -> bool {
        let mut held = match self.held.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let age_ms = now_ms.saturating_sub(self.last_tick_ms());
        if held.is_none() || age_ms <= bound.as_millis() as u64 {
            return false;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        *held = None;
        self.stood_down.store(true, Ordering::Relaxed);
        drop(held);
        logline!(
            "clauth: pid {} completed no usage tick in {}s: released the usage-fetch lease for \
             another instance to take",
            std::process::id(),
            bound.as_secs()
        );
        true
    }

    /// How many times the watchdog has released the lease.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Epoch ms of the last completed tick, or of the take when none has
    /// completed since.
    pub(crate) fn last_tick_ms(&self) -> u64 {
        self.last_tick_ms.load(Ordering::Relaxed)
    }
}

/// How long a lease holder may go without completing a tick before its
/// watchdog gives the lease up for a standby to take.
pub(crate) const LEASE_STALE_BOUND: Duration = Duration::from_secs(5 * 60);

/// What the lease watchdog carries from one poll to the next: the tick stamp it
/// last read, and the wall time since that stamp a suspend or a clock jump
/// explains. The total restarts whenever the stamp moves, so a sleep before the
/// last completed tick never stretches the bound on a stall after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LeaseWatch {
    stamp_ms: u64,
    unaccounted_ms: u64,
}

/// One lease-watchdog poll: fold this poll's unaccounted wall time
/// ([`super::watchdog_unaccounted`], the daemon watchdog's suspend rule) into
/// the total since the last completed tick, then judge the stamp on the time
/// the tick thread actually had. Pure but for the lease it may release, so the
/// drop decision is pinned with an injected clock.
///
/// The total never exceeds the wall time since the stamp: a poll that holds
/// both a suspend and the first tick after the wake would otherwise charge the
/// whole suspend, though only the part after the stamp is time the stamp could
/// have aged. Both clocks are wall time, so a backward step during a wedge
/// stretches the bound by the step.
pub(crate) fn lease_watchdog_step(
    lease: &FetchLease,
    watch: LeaseWatch,
    slept_from_ms: u64,
    now_ms: u64,
    poll: Duration,
    bound: Duration,
) -> LeaseWatch {
    let stamp_ms = lease.last_tick_ms();
    let carried = if stamp_ms == watch.stamp_ms {
        watch.unaccounted_ms
    } else {
        0
    };
    let unaccounted_ms = carried
        .saturating_add(super::watchdog_unaccounted(
            now_ms.saturating_sub(slept_from_ms),
            poll.as_millis() as u64,
        ))
        .min(now_ms.saturating_sub(stamp_ms));
    lease.release_if_stale(now_ms.saturating_sub(unaccounted_ms), bound);
    LeaseWatch {
        stamp_ms,
        unaccounted_ms,
    }
}

/// Spawn the lease watchdog beside a scheduler's tick thread: every
/// [`super::WATCHDOG_POLL`] it runs [`lease_watchdog_step`] against
/// [`LEASE_STALE_BOUND`], so a holder whose tick thread wedges gives the lease
/// up and a standby takes it at its next 1 s retry. Ends once `shutting_down`
/// is set, as the tick thread does.
pub(crate) fn spawn_lease_watchdog(lease: Arc<FetchLease>, shutting_down: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("clauth-lease-watchdog".into())
        .spawn(move || {
            let mut watch = LeaseWatch::default();
            while !shutting_down.load(Ordering::SeqCst) {
                let slept_from = crate::usage::now_ms();
                std::thread::sleep(super::WATCHDOG_POLL);
                watch = lease_watchdog_step(
                    &lease,
                    watch,
                    slept_from,
                    crate::usage::now_ms(),
                    super::WATCHDOG_POLL,
                    LEASE_STALE_BOUND,
                );
            }
        });
    if let Err(e) = spawned {
        logline!(
            "clauth: failed to spawn the usage-fetch lease watchdog: {e}. A stalled usage tick \
             will keep the lease until this process exits"
        );
    }
}

/// Where the daemon singleton's flock lives. Test-only, so a module outside
/// `daemon` can pose a held or unreadable lock without [`super::LOCK_FILE`]
/// leaving the subsystem that owns it.
#[cfg(test)]
pub(crate) fn daemon_lock_path() -> std::path::PathBuf {
    clauth_dir().expect("clauth dir").join(super::LOCK_FILE)
}

/// Open + exclusively lock the daemon singleton, standing in for a live daemon.
/// Test-only, and shared by every module that needs one: it goes through
/// [`crate::profile::open_state_file`] exactly as [`claim_once`] does, so a
/// fixture cannot drift from the file mode production creates. The handle must
/// stay in scope — the flock releases when it drops.
#[cfg(test)]
pub(crate) fn hold_daemon_lock() -> File {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file =
        crate::profile::open_state_file(&dir.join(super::LOCK_FILE)).expect("open the daemon lock");
    file.try_lock().expect("acquire the free daemon lock");
    file
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_probe.rs"]
mod tests;
