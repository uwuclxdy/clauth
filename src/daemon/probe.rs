//! Daemon presence + the single-fetcher lease (dual-scheduler dedup, #27).
//!
//! Three `~/.clauth` flock files, peers in the same dir:
//!   * `clauthd.lock` — the daemon singleton + **presence beacon**. Held for
//!     life by the running daemon; a display-only try-lock tells the TUI header
//!     whether a daemon is up (an advisory lock auto-releases on process death,
//!     so a dead daemon reads as absent on the next probe). Paired with
//!     `status.json`'s freshness it drives the `● daemon` health dot
//!     ([`daemon_health`]). [`singleton_held`] reads the same lock as a decision
//!     for `clauth daemon --status`, where not knowing has to be an error rather
//!     than a hidden dot.
//!   * `clauthd-standby.lock` — the **standby slot** ([`StandbySlot`], #57). One
//!     waiter may park on the singleton lock; every later instance is
//!     [`Claim::Redundant`] and exits, so a spawner that fires repeatedly can no
//!     longer pile up parked daemons.
//!   * `usage-fetch.lock` — the **single-fetcher lease** ([`FetchLease`]).
//!     Exactly one instance (daemon OR a TUI) holds it at a time and does all
//!     usage fetching / rotation / switch decisions while it does; every other
//!     instance hydrates from the shared disk cache. First-come, held for life,
//!     released only on process exit — no preemption, so the switch-decider
//!     never thrashes between processes.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::profile::clauth_dir;

/// How stale `status.json` may be before the `● daemon` dot flips green→amber.
/// The daemon stamps it every ~1s loop tick, but a single tick can legitimately
/// block up to the keychain shell-outs' 20s total kill deadline (a
/// read-modify-write at 10s each), so the window rides just above that. It also lands at the daemon's tightened
/// [`WATCHDOG_DEADLINE`](super::WATCHDOG_DEADLINE), so amber reads as "wedging,
/// about to be aborted + restarted" rather than a transient slow tick.
const DAEMON_STALE_MS: u64 = 30_000;

/// The `● daemon` header dot's three display states, derived from the daemon
/// singleton flock (presence) + the `generated_at` stamp inside `status.json`
/// (health — the daemon's own write time, not the file's mtime). Nothing here
/// gates fetching — that is [`FetchLease`] — but `clauth daemon --status` does
/// answer a caller's spawn-or-not off it, and the probe TAKES the flock it
/// tests, so a concurrent claim must survive that (see [`CLAIM_ATTEMPTS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonHealth {
    /// No daemon: `clauthd.lock` is free (never started, or the holder died).
    /// The dot is hidden; the TUI self-fetches under its own lease.
    Absent,
    /// A daemon holds the lock but its feed is stale/unwritten — wedging,
    /// pre-abort, or just-booted before the first `status.json` write. Amber.
    Stale,
    /// A daemon is up and its feed is fresh. Green.
    Fresh,
}

/// Probe the daemon's presence + health for the header dot. Best-effort: any
/// error that hides whether a daemon is up reads as [`DaemonHealth::Absent`]
/// (the dot simply disappears — never a false "daemon up"). Never CREATES the
/// lock file: a missing file means no daemon has ever started here.
pub(crate) fn daemon_health() -> DaemonHealth {
    let Ok(dir) = clauth_dir() else {
        return DaemonHealth::Absent;
    };
    let Ok(lock_file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(super::LOCK_FILE))
    else {
        return DaemonHealth::Absent;
    };
    match lock_file.try_lock() {
        // We took it → nobody holds it → no live daemon. Dropping releases it.
        Ok(()) => return DaemonHealth::Absent,
        // Held → a daemon is present; fall through to the health read.
        Err(std::fs::TryLockError::WouldBlock) => {}
        // Can't tell (io error): hide the dot rather than assert a daemon.
        Err(std::fs::TryLockError::Error(_)) => return DaemonHealth::Absent,
    }
    // Present. A missing/unreadable feed = booted-but-not-yet-published → amber.
    let Ok(body) = std::fs::read_to_string(dir.join(super::STATUS_FILE)) else {
        return DaemonHealth::Stale;
    };
    if status_is_fresh(&body, crate::usage::now_ms()) {
        DaemonHealth::Fresh
    } else {
        DaemonHealth::Stale
    }
}

/// Pure freshness test: `body`'s `generated_at` stamp is within [`DAEMON_STALE_MS`]
/// of `now_ms`. An unparseable body or a missing/malformed stamp reads as stale —
/// never render a feed we can't read as fresh. A stamp in the FUTURE counts as
/// fresh (clock skew must not flap the dot).
pub(crate) fn status_is_fresh(body: &str, now_ms: u64) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(stamp) = v.get("generated_at").and_then(|g| g.as_str()) else {
        return false;
    };
    let Some(secs) = crate::usage::iso_to_epoch_secs(stamp) else {
        return false;
    };
    if secs < 0 {
        return false;
    }
    let generated_ms = (secs as u64).saturating_mul(1000);
    now_ms.saturating_sub(generated_ms) <= DAEMON_STALE_MS
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
/// header, 1 Hz), [`singleton_held`] (`clauth daemon --status` at whatever rate a
/// supervisor polls, plus once per `clauth start --with-fallback`, which refuses
/// when no daemon is there to decide its switches) and [`standby_waiting`] (the
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

/// How long `--replace` waits for a terminated holder's flock to auto-release on
/// death before it escalates (SIGTERM → SIGKILL) and, after the escalation,
/// before it gives up. A dying process releases its advisory flock within a
/// handful of scheduler ticks; 5 s is generous headroom over that.
const REPLACE_WAIT: Duration = Duration::from_secs(5);
/// Poll spacing while `--replace` waits for the freed flock. Two orders of
/// magnitude below [`REPLACE_WAIT`], well under any human-visible delay.
const REPLACE_POLL: Duration = Duration::from_millis(50);

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
             assignment drops the old value AFTER claim_once has already run — the next attempt \
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
/// confirm. Otherwise it SIGTERMs the holder, waits for the flock to
/// auto-release on death (bounded), escalates to SIGKILL once, then claims. The
/// identity guard narrows the recycled-pid window [`holder_pid`] documents by
/// requiring the pid to still be a running `clauth daemon` (not merely a clauth
/// process — `clauth start`/`mcp`/`tui` share the binary name), so a stale pid
/// recycled onto an unrelated process, or onto another clauth subcommand, is
/// never signalled.
pub(crate) fn claim_by_replacing(dir: &Path) -> Result<Claim> {
    claim_by_replacing_with(dir, REPLACE_WAIT, REPLACE_POLL)
}

/// [`claim_by_replacing`] with the wait schedule injected, so a test can pin the
/// poll loop and the escalation without sleeping the full [`REPLACE_WAIT`].
/// Always yields [`Claim::Active`] on success; every failure mode is an `Err`,
/// never a silent [`Claim::Redundant`] that would log-and-exit-0 on the caller.
pub(crate) fn claim_by_replacing_with(dir: &Path, wait: Duration, poll: Duration) -> Result<Claim> {
    claim_by_replacing_retry_with(dir, wait, poll, CLAIM_ATTEMPTS, CLAIM_RETRY)
}

/// [`claim_by_replacing_with`] with the retry schedule injected, so a test can
/// pin the probe-collision recovery without sleeping for it.
pub(crate) fn claim_by_replacing_retry_with(
    dir: &Path,
    wait: Duration,
    poll: Duration,
    attempts: u32,
    retry: Duration,
) -> Result<Claim> {
    // Fast path: no daemon → a normal start. Retry past transient probe holds
    // (TUI header at 1 Hz, clauth daemon --status) that take the flock and
    // release it microseconds later — a real holder keeps its lock for the
    // process lifetime, so anything that clears on retry was a reader.
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
    // If the retry cleared the transient, try a normal claim. If a daemon wins
    // the lock in the instant between the presence check and the claim, fall
    // through and replace it rather than returning a silent `Redundant` (which
    // `serve` would log and exit 0 on, leaving the operator's upgrade un-started).
    if !held && let Claim::Active(lock) = claim_once(dir, false)? {
        return Ok(Claim::Active(lock));
    }
    let Some(pid) = holder_pid() else {
        anyhow::bail!(
            "a clauth daemon is running but its pid is unreadable; kill it manually, then start"
        );
    };
    if !pid_is_clauth_daemon(pid) {
        anyhow::bail!(
            "the recorded daemon pid {pid} is not a running clauth daemon (it exited during \
             handover or was recycled); re-run once it settles, or kill the daemon manually"
        );
    }
    let sent_term = terminate_pid(pid, false);
    if let Some(lock) = wait_for_active(dir, wait, poll) {
        return Ok(Claim::Active(lock));
    }
    // A graceful term didn't free the lock in time: force-kill, wait once more.
    let sent_kill = terminate_pid(pid, true);
    if let Some(lock) = wait_for_active(dir, wait, poll) {
        return Ok(Claim::Active(lock));
    }
    if !sent_term && !sent_kill {
        anyhow::bail!(
            "could not signal the running clauth daemon (pid {pid}): the `kill` command is not on \
             PATH; kill it manually, then start"
        );
    }
    anyhow::bail!(
        "the running clauth daemon (pid {pid}) did not release the lock within {}s of SIGKILL; \
         it may be wedged uninterruptibly",
        wait.as_secs()
    )
}

/// Poll the singleton lock until this process can claim it, up to `wait`. Returns
/// the held [`DaemonLock`] on success (its pid stamped by [`DaemonLock::active`]),
/// or `None` on timeout. The flock a dying holder released is what this waits on,
/// so the loop is load-bearing: the first attempt races the death and normally
/// still reads the lock held.
fn wait_for_active(dir: &Path, wait: Duration, poll: Duration) -> Option<DaemonLock> {
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(Claim::Active(lock)) = claim_once(dir, false) {
            return Some(lock);
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
/// but the process survived"). Shelling out keeps `unsafe_code = "forbid"` intact
/// (`libc::kill` would need `unsafe`, and `signal_hook` only *receives*), matching
/// the `/usr/bin/security` shell-out pattern already in the crate. A non-zero exit
/// (a dead pid's `ESRCH`) still counts as run: the caller polls the flock either
/// way. Long-form flags so the call site documents itself.
#[cfg(unix)]
fn terminate_pid(pid: u32, hard: bool) -> bool {
    let signal = if hard { "KILL" } else { "TERM" };
    std::process::Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .is_ok()
}

#[cfg(windows)]
fn terminate_pid(pid: u32, hard: bool) -> bool {
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string()]);
    // A console daemon has no window to accept the graceful WM_CLOSE, so the
    // first (soft) pass usually no-ops and the caller escalates here with /F.
    if hard {
        cmd.arg("/F");
    }
    cmd.status().is_ok()
}

/// Confirm `pid` is still a running `clauth daemon` before `--replace` signals
/// it, narrowing the recycled-pid window [`holder_pid`] documents. The check is
/// on the daemon ROLE, not the binary name: `clauth start` (resident around a
/// live Claude Code session), `clauth mcp`, and `clauth tui` all share
/// `comm == "clauth"`, so a name-only guard would let `--replace` SIGKILL one of
/// them if the stale pid recycled onto it. Requiring argv `clauth daemon …`
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
/// fully written. Informational only — its one caller reaches it past a true
/// [`singleton_held`], and the header dot answers off [`daemon_health`], so
/// presence is proven by the flock either way and a pid left behind by a dead
/// daemon is never read as one being up.
///
/// One residual, accepted: the newline sentinel rules out a TORN stamp, not a
/// complete stale one. Between a successor winning the flock and its
/// [`stamp_pid`] reaching `set_len(0)` — a handful of instructions — the dead
/// predecessor's pid is still whole and readable, so `--status` can print a pid
/// the OS has since recycled onto something unrelated. The sidecar is never read
/// unless [`singleton_held`] is already true, so a lingering pid from a fully
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

/// The single-fetcher lease over `usage-fetch.lock` (#27). Once acquired the
/// [`File`] is retained for the process lifetime, so the flock is held for life
/// and released only on exit — deliberately NOT re-acquired per tick, which
/// would bounce the switch-decider between processes (the #27 switch thrash).
///
/// The inner [`Mutex`] is a pure leaf: locked only inside [`acquire`](Self::acquire)
/// and released before it returns, so it carries no lock rank. The flock itself
/// sits outside the compile-time rank system and must be taken OUTERMOST of the
/// tick's ranked locks (`acquire` runs at the very top of the tick).
#[derive(Default)]
pub(crate) struct FetchLease {
    held: Mutex<Option<File>>,
}

impl FetchLease {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Become — or confirm we already are — the single fetcher. Returns `true`
    /// when THIS instance holds the lease (fetch this tick), `false` when another
    /// instance holds it or the lock is unreadable (stand down and hydrate from
    /// the shared cache). Idempotent once held: a held lease short-circuits to
    /// `true`, so the flock is never re-taken. Both error arms of the try-lock
    /// stand down — an io error is never a licence to dup-fetch.
    pub(crate) fn acquire(&self) -> bool {
        let mut held = match self.held.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if held.is_some() {
            return true;
        }
        let Ok(dir) = clauth_dir() else {
            return false;
        };
        // The lease file lives in `~/.clauth`; ensure the dir exists (best-effort,
        // 0o700) so a first-run TUI with no daemon can still take the lease.
        // `serve()` already creates it for the daemon; a TUI normally reaches here
        // with the dir already present (profiles live in it).
        let _ = crate::profile::mkdir_700(&dir);
        let Ok(file) = crate::profile::open_state_file(&dir.join(super::FETCH_LOCK_FILE)) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                *held = Some(file);
                true
            }
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(_)) => false,
        }
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
