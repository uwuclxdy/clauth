//! `clauth daemon` — headless scheduler owner.
//!
//! Runs the exact same background refresher the TUI runs (`spawn_refresher`),
//! but with no ratatui loop. Its jobs each tick:
//!   1. execute any auto-switch the scheduler queued (`pending_switch` /
//!      `pending_switch_off`) — this is what makes unattended auto-switch work
//!      with the TUI closed, the operator's core requirement;
//!   2. rewrite `~/.clauth/status.json` atomically (the external read feed);
//!   3. pick up external config changes (a new `clauth login`, a TUI edit).
//!
//! The scheduler already persists `usage_cache.json` inside `apply_outcome`, so
//! the daemon and the TUI share one cache. A single-instance advisory lock keeps
//! two schedulers from double-firing.

pub(crate) mod api;
#[cfg(unix)]
pub(crate) mod bridge;
pub(crate) mod gateway;
pub(crate) mod log_rotate;
mod probe;
pub(crate) mod proxies;
mod status_json;

#[cfg(test)]
#[path = "../../tests/inline/proxy_p6_feed.rs"]
pub(crate) mod proxy_p6_feed_tests;
mod tick;
mod types;

#[cfg(test)]
pub(crate) use probe::LEASE_STALE_BOUND;
/// The running daemon's pid sidecar reader, shared by the gateway hold writer
/// in `crate::gateway`.
pub(crate) use probe::holder_pid;
use probe::{Claim, DaemonLock, StandbySlot, claim_singleton};
/// The single-fetcher lease + the header chip's daemon presence/health probe
/// (dual-scheduler dedup, #27).
pub(crate) use probe::{
    DaemonHealth, FetchLease, LeaseTicket, daemon_health, gateway_slot, probe_daemon, proxy_slots,
    singleton_held, spawn_lease_watchdog,
};
/// The TUI's `stop daemon`: `--replace`'s termination with no claim after it.
pub(crate) use probe::{DaemonStop, stop_running};
#[cfg(test)]
pub(crate) use probe::{daemon_lock_path, feed_reads, hold_daemon_lock, reset_feed_reads};
/// Small daemon state types + the backoff schedule, re-exported so callers keep
/// referencing them as `super::…` after the extraction.
pub(crate) use types::{SwitchBackoff, switch_backoff_ms};

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::claude::link_profile_credentials;
use crate::lockorder::RankedMutex;
use crate::logline::logline;
use crate::out::outln;
use crate::profile::{
    AppConfig, ConfigHandle, ProfileName, ReloadFingerprint, atomic_write_600, clauth_dir,
    load_config, mkdir_700, reload_fingerprint,
};
use crate::usage::{
    ActivityStore, ClaudeResetsMemoLock, CodexResetsMemoLock, FetchStatus, KickBlocks,
    LastFetchedAt, LegKey, NextRefreshPerProfile, PendingSwitch, PendingSwitchOff, PollStreaks,
    RefetchQueue, SkippedUnreadable, StatusStore, SuppressedAuthExpiredStore, ThirdPartyBroken,
    ThirdPartyList, ThirdPartyStatusStore, ThirdPartyStreaks, ThirdPartyUsageStore, TokenList,
    UsageInfo, UsageStore, WeeklyResetKicks, bootstrap_fetch, bootstrap_third_party,
    collect_oauth_seed_names, collect_third_party_entries, collect_tokens, new_claude_resets_memo,
    new_codex_resets_memo, spawn_refresher,
};
use status_json::LiveSignals;
// `clauth list` (src/list.rs) renders a human table over the same body, so the
// two surfaces read one code path and cannot drift.
pub(crate) use status_json::{
    ProfileEntry, build_codex_entries, build_profile_entries, build_status,
};
// The feed's schema number, published by `GET /api/v1/health` so a remote reader
// can refuse a daemon newer than it knows (wiki/Daemon.md's evolution rule).
pub(crate) use status_json::SCHEMA_VERSION;

/// Main-loop cadence. The scheduler ticks on its own 1s timer; this loop only
/// executes queued switches/config edits and rewrites `status.json`, so 1s is plenty.
const TICK: Duration = Duration::from_secs(1);
/// How often (in `TICK`s) the run loop checks `daemon.log` for size-capping
/// ~5 min at the 1s tick — rare enough to be free, frequent enough to
/// bound a busy log.
const LOG_ROTATE_EVERY_TICKS: u64 = 300;
/// How often (in `TICK`s) the tick reads the bare macOS Keychain item for a
/// login another account's Claude Code wrote there
/// (`claude::correct_bare_keychain_item_detached`): one `security` read per ~30 s, where
/// every tick would spend one per second.
#[cfg(target_os = "macos")]
const BARE_ITEM_SCAN_EVERY_TICKS: u64 = 30;
/// How often (in `TICK`s) the tick sweeps live sessions' per-session Keychain
/// items for a stale copy of a rolling bearer that no stamp will re-point
/// (`runtime::sweep_stale_copied_bearers_detached`): at boot, then every
/// ~5 min. Each sweep reads only the items of sessions that lost their
/// `.credentials.json`, one `security` read each.
#[cfg(target_os = "macos")]
const STALE_COPY_SWEEP_EVERY_TICKS: u64 = 300;
const STATUS_FILE: &str = "status.json";
const LOCK_FILE: &str = "clauthd.lock";
/// The live daemon's pid, an UNLOCKED peer of [`LOCK_FILE`]. Kept out of the
/// lock file itself because Windows locks are mandatory (`LockFileEx`): a
/// `--status` reader in another process cannot read bytes inside the daemon's
/// held exclusive lock, so the pid has to live somewhere unlocked. Informational
/// only — presence is the flock, never this file. See [`probe::holder_pid`].
const PID_FILE: &str = "clauthd.pid";
/// The standby slot's flock (#57). A peer of [`LOCK_FILE`]; held by the single
/// instance allowed to park on the singleton lock. See [`probe::Claim`].
const STANDBY_LOCK_FILE: &str = "clauthd-standby.lock";
/// The single-fetcher lease file (#27). A peer of [`LOCK_FILE`] in `~/.clauth`,
/// held by whichever instance (daemon or a TUI) is the current usage fetcher
/// while its ticks keep completing. See [`FetchLease`](probe::FetchLease).
const FETCH_LOCK_FILE: &str = "usage-fetch.lock";
/// The daemon's log beside `status.json`: its stderr and stdout when the TUI
/// starts it. The run loop caps it by this name.
const LOG_FILE: &str = "daemon.log";

/// Latch for the retention leg's check-failed line, so a standing failure is
/// reported once per failure run rather than on every tick. Cleared by the
/// next clean check, so a recurrence is reported again.
static RETENTION_CHECK_FAILED: AtomicBool = AtomicBool::new(false);

/// The retention leg's cadence: a change event under `~/.claude` drives the
/// check, and the 30 s fallback (the polling cadence too, when no watch arms)
/// covers an event the watcher never delivered.
const RETENTION_TIMINGS: crate::watchdog::Timings = crate::watchdog::Timings {
    debounce: Duration::from_millis(200),
    cooldown: Duration::from_millis(500),
    fallback: Duration::from_secs(30),
    config_poll: Duration::from_secs(30),
    credential_poll: Duration::from_secs(30),
    swap_poll: Duration::from_secs(30),
};

/// One retention check for the leg: run [`crate::retention::reconcile`] and
/// latch its failure line, so a standing failure is reported once per failure
/// run. Cleared by the next clean check, so a recurrence is reported again.
/// Extracted from the `serve` wiring so the latch has a test.
fn retention_check() {
    if let Err(e) = crate::retention::reconcile() {
        if !RETENTION_CHECK_FAILED.swap(true, Ordering::Relaxed) {
            logline!("clauth daemon: cleanupPeriodDays check failed: {e:#}");
        }
    } else {
        RETENTION_CHECK_FAILED.store(false, Ordering::Relaxed);
    }
}

/// The retention leg as a watchdog reconcile: `check` is its config leg, and
/// it has no credential or swap leg.
struct RetentionCheck<F>(F);

impl<F: Fn()> crate::watchdog::Reconcile for RetentionCheck<F> {
    fn config(&self) {
        (self.0)();
    }
    fn credentials(&self) {}
    fn swap_poll(&self) {}
}

/// Run `check` once now, then on every change under `claude_home`, on the
/// watchdog's event loop with its polling fallback, until `shutdown` fires or
/// disconnects. The session watchdogs put a lost `cleanupPeriodDays` back only
/// while a `clauth start` session runs; this leg covers a bare `claude` with
/// no session live. The watch arms before the thread spawns, so a write
/// landing right after the spawn still raises its event.
///
/// Every child counts as a match rather than just `settings.json`: the file is
/// legitimately absent, and a name-only interest would read every other
/// `~/.claude` write as an unmatched event, so the dead-filter check would
/// report a healthy watcher. An open of a child wakes nothing.
fn spawn_retention_watch(
    claude_home: &std::path::Path,
    timings: crate::watchdog::Timings,
    shutdown: crossbeam_channel::Receiver<()>,
    check: impl Fn() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let specs = [crate::watchdog::WatchSpec::new(
        claude_home,
        crate::watchdog::Interest::AnyChild,
    )];
    let watcher = crate::watchdog::try_start(&specs, timings.debounce);
    std::thread::Builder::new()
        .name("clauth-retention".into())
        .spawn(move || {
            check();
            crate::watchdog::run_with_watcher(
                watcher,
                specs.len(),
                &shutdown,
                &timings,
                &RetentionCheck(check),
            );
        })
}

/// Anti-wedge watchdog: abort if no tick completes within this window.
/// `TICK` is 1s, so ~30 missed ticks. A `StateLock` flock wait bounds out at
/// 25 s and a switch runs a `/usr/bin/security` subprocess inside the hold,
/// so a stuck keychain or a wedged flock holder can freeze the single-threaded
/// run loop past this window; `std::process::abort()` then lets launchd's
/// `KeepAlive{SuccessfulExit=false}` restart the daemon (boot()'s relink +
/// atomic writes make restart safe).
///
/// Tightened 60s→30s for the single-fetcher lease (#27): a main loop wedged alive
/// would otherwise delay every other instance's fetch for as long as the process
/// lives — the scheduler thread may still be completing ticks, so its lease
/// watchdog cannot see this wedge — 30s frees the lease about as fast as the
/// retired TUI freshness re-arm did. A wedged SCHEDULER thread is that watchdog's
/// to break (SCOPE below). TENSION: a legit switch's keychain shell-outs can block
/// inside the `StateLock`, leaving only ~10s of slack. Twice now the count of those
/// shell-outs grew under this deadline (a read-modify-write made one mirror two
/// calls; a first-login adopt makes one switch two mirrors), so the bound is no
/// longer per-call: `lock::SUBPROCESS_BUDGET` caps everything ONE state-lock
/// hold spends in `security` at 20s in aggregate. If it ever false-aborts,
/// shrink THAT (the real fix), do NOT loosen this deadline — the lease's
/// wedged-daemon recovery depends on it. One `lock::SharedSubprocessBudget`
/// spans the WHOLE tick (`tick.rs` arms it before both drains): its window
/// also caps each drain's flock wait, and the tick skips its next drain once
/// the window or this deadline is spent, so a tick's shell-outs and flock
/// waits together stay under this deadline (pre-fix, two wedged drains
/// waited 2 × `STATE_LOCK_TIMEOUT` = 50 s here).
///
/// SCOPE: `heartbeat` is stamped by the MAIN loop only, so this covers a wedged
/// main loop. A wedged SCHEDULER thread (which is what actually holds the lease)
/// keeps the main loop ticking and the feed fresh, so it trips nothing here; the
/// scheduler's own lease watchdog ([`probe::spawn_lease_watchdog`]) releases
/// the lease once its tick has completed nothing for
/// [`probe::LEASE_STALE_BOUND`]. Do not read this deadline as covering the
/// fetch path itself.
///
/// A SUSPENDED process (Modern Standby/S0ix, S3, a VM pause, SIGSTOP) freezes
/// the loop but not the wall clock, so a freeze reads as a wedge on wall time
/// alone; the watchdog therefore charges a poll that overshot its window by
/// more than [`WATCHDOG_POLL_SLACK`] no running time
/// ([`watchdog_unaccounted`]) and judges the gap on time the loop actually
/// had. A legal slow tick (up to this deadline minus `TICK` of budget)
/// survives a freeze mid-tick, and a loop that wedges before or after the
/// resume still aborts within the deadline of running time. Ceiling: a box
/// that cannot run the watchdog thread for one poll plus the slack at a
/// stretch degrades stall detection with the thread itself.
const WATCHDOG_DEADLINE: Duration = Duration::from_secs(30);
/// How often the watchdog re-checks the tick heartbeat.
const WATCHDOG_POLL: Duration = Duration::from_secs(10);
/// How far past [`WATCHDOG_POLL`] a poll may run before it reads as
/// interrupted (a suspend, a wall-clock jump) rather than as scheduling
/// jitter. Below the slack only the overshoot is unaccounted; at or past it
/// the whole poll charges no running time.
const WATCHDOG_POLL_SLACK: Duration = Duration::from_secs(1);

/// One watchdog evaluation: if the main loop last completed a tick more than
/// `deadline_ms` ago, invoke `on_stall`. Production passes `std::process::abort`;
/// tests inject a flag/panic. A zero `last_tick_ms` (no tick yet — boot in
/// progress) never trips. Pure so the abort decision is unit-testable.
fn watchdog_check(last_tick_ms: u64, now_ms: u64, deadline_ms: u64, on_stall: impl FnOnce()) {
    if last_tick_ms != 0 && now_ms.saturating_sub(last_tick_ms) > deadline_ms {
        on_stall();
    }
}

/// The wall time one poll leaves unaccounted: past [`WATCHDOG_POLL_SLACK`] the
/// poll was interrupted by a suspend or a wall-clock jump and charges no
/// running time at all (the loop froze with the clock); a poll near its
/// window charges the window, its scheduling jitter excluded. Pure so the
/// accounting is unit-testable.
fn watchdog_unaccounted(elapsed_ms: u64, poll_ms: u64) -> u64 {
    if elapsed_ms > poll_ms.saturating_add(WATCHDOG_POLL_SLACK.as_millis() as u64) {
        elapsed_ms
    } else {
        elapsed_ms.saturating_sub(poll_ms)
    }
}

/// One watchdog round: fold this poll's unaccounted time into the total and
/// run the stall check against the running-time clock (`now - unaccounted`).
/// Returns the updated total. Pure so the absorb-and-check composition is
/// unit-testable; production passes `std::process::abort` as `on_stall`.
fn watchdog_step(
    last_tick_ms: u64,
    slept_from_ms: u64,
    now_ms: u64,
    unaccounted_ms: u64,
    poll_ms: u64,
    deadline_ms: u64,
    on_stall: impl FnOnce(),
) -> u64 {
    let unaccounted = unaccounted_ms.saturating_add(watchdog_unaccounted(
        now_ms.saturating_sub(slept_from_ms),
        poll_ms,
    ));
    watchdog_check(
        last_tick_ms,
        now_ms.saturating_sub(unaccounted),
        deadline_ms,
        on_stall,
    );
    unaccounted
}

/// Whether the next drain must be skipped this tick: the tick's shared window
/// is spent (a `Some(0)` remainder — the window flock waits and keychain
/// shell-outs share), or the watchdog deadline is reached. A drain handed a
/// wait past either point can only time out or abort the daemon mid-switch.
/// Pure, so both triggers pin without posing real waits.
fn drains_exhausted(remaining: Option<Duration>, now: Instant, deadline: Instant) -> bool {
    remaining.is_some_and(|r| r.is_zero()) || now >= deadline
}

/// The descriptors the re-exec passes to the next image of this process: the
/// open file description holding the singleton's flock (`clauthd.lock`) and,
/// when `--listen` runs, the bound REST listener. std opens every descriptor
/// close-on-exec, so without a handover the exec would drop both between the
/// old image's exec and the new image's claim: a parked `--standby` could
/// take the singleton there, and any process could take the port. The old
/// image clears close-on-exec on each registered fd right before the exec
/// and names its number in the exec command's env — never this process's,
/// which has live threads at the exec; the new image reads the vars first
/// thing, re-marks the adopted fds close-on-exec so no child inherits them,
/// validates each number while borrowing it (the lock file by device and
/// inode plus the flock itself, a stream socket by its type and bound
/// address), and adopts the validated descriptors instead of claiming and
/// binding afresh. A number that fails validation is left open (never
/// closed) and logged per descriptor; a parsed number at or above the stdio
/// floor that is still open is re-marked close-on-exec, so no later child
/// inherits it, and the start falls back to the fresh claim and bind.
#[cfg(unix)]
pub(crate) mod exec_handover {
    use std::fs::File;
    use std::net::{SocketAddr, TcpListener};
    use std::os::fd::{FromRawFd, RawFd};
    use std::path::Path;
    use std::sync::Mutex;

    use crate::logline::logline;

    /// The env var naming the lock descriptor's fd to the next image.
    pub(crate) const LOCK_FD_ENV: &str = "CLAUTH_EXEC_LOCK_FD";
    /// The env var naming the REST listener's fd to the next image.
    pub(crate) const LISTEN_FD_ENV: &str = "CLAUTH_EXEC_LISTEN_FD";

    /// The fd of the open file description holding the singleton's flock,
    /// once this image holds the lock.
    static LOCK_FD: Mutex<Option<RawFd>> = Mutex::new(None);
    /// The bound REST listener's fd, once this image serves the API.
    static LISTEN_FD: Mutex<Option<RawFd>> = Mutex::new(None);
    /// The listener [`take_at_start`] validated and stored, for
    /// [`take_listener`].
    static ADOPTED_LISTENER: Mutex<Option<TcpListener>> = Mutex::new(None);

    /// The descriptors [`prepare_for_exec`] cleared close-on-exec on, for
    /// the exec command to name in the next image's env.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) struct Handover {
        lock: Option<RawFd>,
        listen: Option<RawFd>,
    }

    /// Record the singleton lock's descriptor once this image holds it, so
    /// the next re-exec hands it over.
    pub(crate) fn register_lock(fd: RawFd) {
        *LOCK_FD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fd);
    }

    /// Record the bound REST listener's descriptor once it is bound, so the
    /// next re-exec hands it over.
    pub(crate) fn register_listener(fd: RawFd) {
        *LISTEN_FD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fd);
    }

    /// Hand the registered descriptors to the next image, right before the
    /// exec: clear close-on-exec on each and return the pair for
    /// [`apply_handover`] to name on the exec command. The process
    /// environment is never mutated: the daemon has live threads at the
    /// exec (on macOS the detached keychain worker reads the env), and
    /// edition 2024 forbids calling an env function while another thread
    /// may. No rollback is needed — a successful exec replaces the image,
    /// and an exec that returned reaches the abort.
    pub(crate) fn prepare_for_exec() -> Handover {
        let lock = *LOCK_FD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(fd) = lock {
            set_cloexec(fd, false);
        }
        let listen = *LISTEN_FD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(fd) = listen {
            set_cloexec(fd, false);
        }
        Handover { lock, listen }
    }

    /// Name the prepared descriptors on the exec command, so the next image
    /// inherits them through the exec: `Command::env` builds the child's
    /// environment from this process's without mutating it, which is what
    /// keeps the live threads racing no env write. The new image's first
    /// act removes both vars before it spawns anything.
    pub(crate) fn apply_handover(command: &mut std::process::Command, handover: Handover) {
        if let Some(fd) = handover.lock {
            command.env(LOCK_FD_ENV, fd.to_string());
        }
        if let Some(fd) = handover.listen {
            command.env(LISTEN_FD_ENV, fd.to_string());
        }
    }

    /// Clear every registration and adopted descriptor, for a test that
    /// registered scratch descriptors it then drops before the process
    /// ends: a later production handover must never touch a recycled fd
    /// number.
    #[cfg(all(test, unix))]
    pub(crate) fn reset() {
        *LOCK_FD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *LISTEN_FD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *ADOPTED_LISTENER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Whether a refused number was open when its own probe ran: a closed
    /// number has no close-on-exec flag to restore.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Refused {
        Open,
        Closed,
    }

    /// Classify a failed libc probe on a raw number: `EBADF` means the
    /// number is closed; any other failure means it answered and is open.
    fn probe_refused() -> Refused {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF) {
            Refused::Closed
        } else {
            Refused::Open
        }
    }

    // Counts this thread's refusal-arm close-on-exec re-marks, so a test
    // can pin that a closed number is never re-marked and an open one is.
    #[cfg(test)]
    thread_local! {
        static REFUSED_REMARKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The re-marks counted on this thread so far.
    #[cfg(test)]
    pub(crate) fn refused_remarks() -> usize {
        REFUSED_REMARKS.with(std::cell::Cell::get)
    }

    /// Undo the handover's close-on-exec clear on a refused number whose
    /// probe found it open, so no later child inherits it.
    fn remark_refused(fd: RawFd) {
        set_cloexec(fd, true);
        #[cfg(test)]
        REFUSED_REMARKS.with(|count| count.set(count.get() + 1));
    }

    /// The new image's first act: read the handover env (removing both vars
    /// at once, so no child of this image inherits them), and validate each
    /// number while borrowing it: the lock fd must be an open handle to
    /// `dir/clauthd.lock` (same device and inode) whose flock this process
    /// holds, and the listener fd, when `listen` is configured, must be a
    /// stream socket bound to that address. A number that fails validation
    /// (an unparsable value, one below 3, a closed or foreign descriptor)
    /// is left open — it may be one this process does not own — and, when it
    /// parsed to a number at or above the stdio floor and is still open,
    /// re-marked close-on-exec, undoing the handover's clear so no later
    /// child inherits it; its refusal is logged with the fallback that runs
    /// for it. Returns the adopted lock (the caller then skips the claim),
    /// or `None` for a fresh claim.
    #[expect(
        unsafe_code,
        reason = "Rust 2024 makes env removal unsafe; this runs as the new image's first act, before any thread or child exists"
    )]
    pub(crate) fn take_at_start(dir: &Path, listen: Option<SocketAddr>) -> Option<File> {
        let lock_env = std::env::var_os(LOCK_FD_ENV);
        let listen_env = std::env::var_os(LISTEN_FD_ENV);
        // SAFETY: `LOCK_FD_ENV` is this handover's own var, and this runs as
        // the new image's first act — before any thread is spawned and
        // before anything can spawn (the boot capture spawns `ps` on macOS) —
        // so no other thread may be inside an env function and no child of
        // this image ever inherits it.
        unsafe { std::env::remove_var(LOCK_FD_ENV) };
        // SAFETY: `LISTEN_FD_ENV` is private to the same handover and is
        // removed in that same pre-thread window, so no child inherits it and
        // no other thread reads the environment here.
        unsafe { std::env::remove_var(LISTEN_FD_ENV) };
        let mut refused_lock = false;
        let lock = match lock_env.as_deref().and_then(handover_fd) {
            Some(fd) => match adopt_lock(fd, dir) {
                Ok(file) => Some(file),
                Err(refused) => {
                    refused_lock = true;
                    // The previous image cleared close-on-exec on this
                    // number right before the exec; a refusal over an open
                    // descriptor leaves it that way, so restore the flag —
                    // an inheritable fd would ride every child of this
                    // image too. A closed number has no flag to restore.
                    if refused == Refused::Open {
                        remark_refused(fd);
                    }
                    None
                }
            },
            None => {
                refused_lock = lock_env.is_some();
                None
            }
        };
        if refused_lock {
            logline!(
                "clauth daemon: the lock descriptor handed over across the re-exec failed validation; claiming the singleton afresh"
            );
        }
        let mut refused_listen = false;
        match listen_env.as_deref().and_then(handover_fd) {
            Some(fd) => {
                let lock_fd = lock.as_ref().map(|file| {
                    use std::os::fd::AsRawFd as _;
                    file.as_raw_fd()
                });
                // The adopted lock's own number is refused before the
                // listener arm runs, so the listener arm can never take
                // ownership of the lock's descriptor — today's validation
                // already refuses the lock's regular file at `SO_TYPE`, but
                // the order keeps a future ownership-taking listener
                // validation from double-owning it.
                let adopted: Result<TcpListener, Refused> = match (listen, lock_fd) {
                    // The adopted lock's own number, open by construction.
                    (Some(_), Some(lock_fd)) if fd == lock_fd => Err(Refused::Open),
                    (Some(addr), _) => adopt_listener(fd, addr),
                    // No listener is configured (unreachable across the
                    // exec, which keeps the argv): a number that was never
                    // validated is not owned, so it stays open.
                    (None, _) => Err(Refused::Open),
                };
                match adopted {
                    Ok(listener) => {
                        *ADOPTED_LISTENER
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(listener);
                    }
                    Err(refused) => {
                        refused_listen = true;
                        // As for the lock arm: the previous image cleared
                        // close-on-exec on this number, and a refusal over
                        // an open descriptor leaves it that way, so restore
                        // the flag — a refused listener keeps its bound
                        // socket open, and without the re-mark it would
                        // also stay inheritable into every later child.
                        if refused == Refused::Open {
                            remark_refused(fd);
                        }
                    }
                }
            }
            None => refused_listen = listen_env.is_some(),
        }
        if refused_listen {
            match listen {
                Some(_) => logline!(
                    "clauth daemon: the listener descriptor handed over across the re-exec failed validation; binding the REST listener afresh"
                ),
                None => logline!(
                    "clauth daemon: the listener descriptor handed over across the re-exec failed validation; no REST listener is configured for this start"
                ),
            }
        }
        lock
    }

    /// Take the listener [`take_at_start`] validated and stored, when it is
    /// the one bound to `listen`; `None` when no listener was handed over
    /// (or it failed validation), so the caller binds afresh. A configured
    /// port 0 matches the bound ephemeral port: the address means "any
    /// port on this ip", never a literal port.
    pub(crate) fn take_listener(listen: SocketAddr) -> Option<TcpListener> {
        let adopted = ADOPTED_LISTENER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()?;
        let bound = adopted.local_addr().ok()?;
        (bound.ip() == listen.ip() && (listen.port() == 0 || bound.port() == listen.port()))
            .then_some(adopted)
    }

    /// Validate one handed-over lock descriptor while borrowing it, then
    /// adopt: an open handle to `dir/clauthd.lock` — same device and inode
    /// — whose flock this process holds. Every check runs through `libc` on
    /// the raw number first, so a refused number (a closed fd, a foreign
    /// descriptor) is left open: it may be one this process does not own,
    /// and closing it (stderr, a supervisor-passed fd) would be worse than
    /// leaving it. Ownership moves in only once validated.
    #[expect(
        unsafe_code,
        reason = "validating a handed-over descriptor needs libc fstat and flock on a raw number, and adopting it needs from_raw_fd"
    )]
    fn adopt_lock(fd: RawFd, dir: &Path) -> Result<File, Refused> {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: `libc::stat` is all integers, so the zeroed value is
        // valid, and no field is read before `fstat` fills the struct.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: `fd` is a number read from the handover env; fstat is
        // defined for any fd number (EBADF for a closed one) and writes only
        // to our own `stat` local.
        if unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
            return Err(probe_refused());
        }
        let expected = std::fs::metadata(dir.join(super::LOCK_FILE)).map_err(|_| Refused::Open)?;
        if (stat.st_dev as u64, stat.st_ino as u64) != (expected.dev(), expected.ino()) {
            return Err(Refused::Open);
        }
        // The flock itself: re-locking the same open file description
        // succeeds only while this process holds it (or takes it, which is
        // the same end state); a refusal means another process holds it and
        // the handover lied.
        // SAFETY: `flock` is defined for any fd number and touches no memory
        // of ours; a closed number fails with EBADF.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Refused::Open);
        }
        // Only now is the number owned: validated as an open handle to the
        // lock file whose flock this process holds.
        // SAFETY: `from_raw_fd` takes ownership of the validated descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        // Re-marked close-on-exec at once: only this image's own next
        // handover clears it, so no child of this image inherits it.
        set_cloexec(fd, true);
        Ok(file)
    }

    /// Validate one handed-over listener descriptor while borrowing it, then
    /// adopt: a stream socket (`SO_TYPE`) bound to the configured address
    /// (`getsockname`), never merely any open descriptor — a configured
    /// port 0 matches the bound ephemeral port, the address meaning "any
    /// port on this ip". The listening flag is checked where the kernel
    /// answers it: linux answers `SO_ACCEPTCONN`, macOS 27 answers every
    /// socket with `ENOPROTOOPT`, so there the type and the bound address
    /// are the validation. A refused number is left open (see
    /// [`adopt_lock`]), so `TcpListener::from_raw_fd` — whose contract is
    /// an open socket — runs only on a validated one.
    #[expect(
        unsafe_code,
        reason = "validating a handed-over listener needs getsockopt on a raw descriptor number, and adopting it needs from_raw_fd"
    )]
    fn adopt_listener(fd: RawFd, listen: SocketAddr) -> Result<TcpListener, Refused> {
        let mut sock_type: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as in [`adopt_lock`], getsockopt is defined for any fd
        // number and writes only to our own locals.
        let type_probe = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut sock_type).cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        if type_probe != 0 {
            return Err(probe_refused());
        }
        if sock_type != libc::SOCK_STREAM {
            return Err(Refused::Open);
        }
        let bound = socket_address(fd);
        let same_address = bound.as_ref().is_some_and(|bound| {
            bound.ip() == listen.ip() && (listen.port() == 0 || bound.port() == listen.port())
        });
        if !same_address {
            return Err(Refused::Open);
        }
        // And a LISTENING socket where the kernel answers the question, so
        // a bound-but-unconnected socket planted in the env never reads as
        // the daemon's listener. A kernel that refuses the read (macOS)
        // leaves the checks above as the validation.
        let mut accepting: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as in [`adopt_lock`], getsockopt is defined for any fd
        // number and writes only to our own locals.
        let ok = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                (&raw mut accepting).cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        if ok == 0 && accepting == 0 {
            return Err(Refused::Open);
        }
        // Only now is the number owned: validated as a stream socket bound
        // to the configured address.
        // SAFETY: `from_raw_fd` takes ownership of the validated socket.
        let listener = unsafe { TcpListener::from_raw_fd(fd) };
        set_cloexec(fd, true);
        Ok(listener)
    }

    /// The address the socket behind `fd` is bound to (`getsockname`), or
    /// `None` when the call fails or the family is not one the daemon
    /// serves.
    #[expect(
        unsafe_code,
        reason = "reading a socket's bound address needs getsockname and a sockaddr_storage reinterpretation no safe std API exposes"
    )]
    fn socket_address(fd: RawFd) -> Option<SocketAddr> {
        // SAFETY: `sockaddr_storage` is plain-integer storage, so the zeroed
        // value is valid and every field read afterwards is one the call
        // wrote.
        let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // SAFETY: `fd` is an open socket (validated by the caller) and the
        // pointers address our own locals; getsockname writes at most `len`
        // bytes into `storage`.
        if unsafe {
            libc::getsockname(
                fd,
                (&raw mut storage).cast::<libc::sockaddr>(),
                &raw mut len,
            )
        } != 0
        {
            return None;
        }
        match libc::c_int::from(storage.ss_family) {
            libc::AF_INET => {
                // SAFETY: the family tags the storage as a sockaddr_in.
                let addr = unsafe { *(&raw const storage).cast::<libc::sockaddr_in>() };
                Some(SocketAddr::from((
                    std::net::Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
                    u16::from_be(addr.sin_port),
                )))
            }
            libc::AF_INET6 => {
                // SAFETY: the family tags the storage as a sockaddr_in6.
                let addr = unsafe { *(&raw const storage).cast::<libc::sockaddr_in6>() };
                Some(SocketAddr::V6(std::net::SocketAddrV6::new(
                    std::net::Ipv6Addr::from(addr.sin6_addr.s6_addr),
                    u16::from_be(addr.sin6_port),
                    addr.sin6_flowinfo,
                    addr.sin6_scope_id,
                )))
            }
            _ => None,
        }
    }

    /// Parse one handover env value into a descriptor number worth probing:
    /// a number below 3 is refused — the daemon's stdio (stderr IS
    /// daemon.log) must never be closed through an adoption — and an
    /// unparsable value is refused too.
    fn handover_fd(value: &std::ffi::OsStr) -> Option<RawFd> {
        value.to_str()?.parse::<RawFd>().ok().filter(|fd| *fd >= 3)
    }

    /// Set (or clear) close-on-exec on `fd`. Explicit on both sides: a raw
    /// descriptor keeps whatever flag it inherited, so adoption sets it and
    /// the handover clears it.
    #[expect(
        unsafe_code,
        reason = "close-on-exec is a raw descriptor flag only fcntl reaches; no safe std API sets it on a number this process may not own"
    )]
    fn set_cloexec(fd: RawFd, on: bool) {
        // SAFETY: `fd` is a descriptor number, not necessarily an open one
        // or one this process owns: fcntl is defined for any number (EBADF
        // for a closed one), and F_GETFD only reads that number's own
        // descriptor flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags != -1 {
            let flags = if on {
                flags | libc::FD_CLOEXEC
            } else {
                flags & !libc::FD_CLOEXEC
            };
            // SAFETY: F_SETFD runs only after F_GETFD found the number open,
            // and it only rewrites that number's own descriptor flags.
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags) };
        }
    }
}

/// Platforms that never re-exec have no descriptors to hand over: the
/// listener slot is always empty, so every start binds afresh.
#[cfg(not(unix))]
pub(crate) mod exec_handover {
    use std::net::{SocketAddr, TcpListener};

    pub(crate) fn take_listener(_listen: SocketAddr) -> Option<TcpListener> {
        None
    }
}

/// The fixed token a handover-capable build answers on stdout, exit 0, to
/// the daemon's hidden capability argument (`EXEC_HANDOVER_FLAG`): the
/// re-exec probe spawns the replacement and compares these exact bytes, so
/// a build that predates the handover (which exits on the unknown
/// argument) or any other program at the path reads as not capable. A
/// build that will serve the API reads its certificate first and answers
/// [`EXEC_HANDOVER_CERT_REFUSED`] with the reason instead.
pub(crate) const EXEC_HANDOVER_TOKEN: &str = "clauth-exec-handover";
/// The marker a replacement that cannot load the API certificate answers
/// instead of the token: the first line of the probe's cert-refused answer,
/// written by the answer arm in `main.rs` and matched by the probe here, so
/// the two never drift apart.
pub(crate) const EXEC_HANDOVER_CERT_REFUSED: &str = "clauth-exec-handover-cert-refused";
/// The probe's argv spelling of the hidden argument, dashes included (the
/// value a spawn receives). `cli.rs` declares the same flag on the
/// `daemon` command without them (`hide = true` keeps it out of --help and
/// the hand-written completions); the parse test drives the real CLI with
/// this string, so a drift between the two reds.
#[cfg(unix)]
pub(crate) const EXEC_HANDOVER_FLAG: &str = "--__exec-handover";

/// Two consecutive reads of the daemon's own executable as replaced make the
/// re-exec due. A rename-replaced file reads whole from its first read (a
/// rename is atomic); the second read gives an install that creates the new
/// file at the path one more tick, and an exec that still fails aborts for
/// the supervisor's restart. Due reports once per streak: a replacement that
/// never clears must not re-log and re-run the check every tick. `None` and
/// `Removed` restart the count — a removed binary has nothing to exec, and
/// the exec failure would abort the daemon. On non-unix it never returns
/// true: Windows keeps the status-feed toast and never restarts itself.
#[derive(Debug, Default)]
pub(crate) struct ReexecWatch {
    #[cfg(unix)]
    replaced_in_a_row: u8,
    /// The identity of the replacement the last not-capable probe stood
    /// down on: while that identity still sits at the recorded path the
    /// watch stays dead (the binary was judged, and re-probing it per
    /// check would spawn once a second forever), and a new identity
    /// re-arms it from two consecutive reads.
    #[cfg(unix)]
    stood_down_identity: Option<BinaryIdentity>,
    /// Whether the last verdict was a stand-down: an unkeyable path (no
    /// identity to compare) then keeps that verdict standing, so the probe
    /// never runs once per due cycle on a file it cannot read.
    #[cfg(unix)]
    stood_down: bool,
}

impl ReexecWatch {
    #[cfg(unix)]
    pub(crate) fn observe(&mut self, change: Option<crate::plugin_host::ExeChange>) -> bool {
        if change == Some(crate::plugin_host::ExeChange::Replaced) {
            // Saturating: a replacement that never clears (an empty stop kit,
            // an old daemon) must neither overflow nor re-report due.
            self.replaced_in_a_row = self.replaced_in_a_row.saturating_add(1);
            return self.replaced_in_a_row == 2;
        }
        self.replaced_in_a_row = 0;
        false
    }

    #[cfg(not(unix))]
    pub(crate) fn observe(&mut self, _change: Option<crate::plugin_host::ExeChange>) -> bool {
        false
    }

    /// Record a not-capable verdict for the replacement at the recorded
    /// path: the streak resets, and the identity is remembered so the same
    /// binary is never re-probed while it still sits there.
    #[cfg(unix)]
    pub(crate) fn stand_down(&mut self, identity: Option<BinaryIdentity>) {
        self.replaced_in_a_row = 0;
        self.stood_down_identity = identity;
        self.stood_down = true;
    }

    /// The identity of the replacement the last stand-down recorded.
    #[cfg(unix)]
    pub(crate) fn stood_down_on(&self) -> Option<BinaryIdentity> {
        self.stood_down_identity
    }

    /// Whether a not-capable verdict stands recorded. A readable file at
    /// the path still re-arms the probe; only an unkeyable one keeps it.
    #[cfg(unix)]
    pub(crate) fn stood_down(&self) -> bool {
        self.stood_down
    }

    /// Reset the streak without a stand-down: the same stood-down binary
    /// still sits at the path, so a NEW replacement (a different identity)
    /// must re-arm from two consecutive reads.
    #[cfg(unix)]
    pub(crate) fn reset_streak(&mut self) {
        self.replaced_in_a_row = 0;
    }
}

/// The watch's own notion of a replacement binary's identity: device,
/// inode, length, mtime at full precision and the mode bits, so an in-place
/// rewrite that keeps the inode and lands in the recorded mtime's own second
/// still reads as a new binary. The mode is part of it because an installer
/// can land the file without its exec bit and chmod it after the probe's
/// spawn was refused: the watch's own replacement reading is mode-blind, so
/// only this identity can re-arm the probe. `None` when the file cannot be
/// vouched for (gone, unreadable, not a regular file): before any
/// stand-down the watch treats the replacement as new, and after one the
/// verdict stands until a keyable file returns.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BinaryIdentity {
    dev_ino: (u64, u64),
    len: u64,
    mtime_nanos: u128,
    mode: u32,
}

#[cfg(unix)]
impl BinaryIdentity {
    fn of(path: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since_epoch| since_epoch.as_nanos());
        Some(Self {
            dev_ino: (meta.dev(), meta.ino()),
            len: meta.len(),
            mtime_nanos,
            mode: meta.mode(),
        })
    }
}

/// How long the capability probe may run before the replacement reads as
/// not capable: the probe's signal is the answer itself (the token, or the
/// cert-refused marker), the wall clock is the backstop for a binary that
/// hangs. The heartbeat is restamped between the probe and the stop, so
/// this bound shares no watchdog window with the stop's serial legs
/// (`the_capability_probe_fits_the_watchdog_deadline`).
#[cfg(unix)]
const EXEC_PROBE_BOUND: Duration = Duration::from_secs(5);

/// The cert-refused answer's first line — [`EXEC_HANDOVER_CERT_REFUSED`]
/// plus the newline — matched whole, so any other non-token answer stays
/// the no-handover verdict.
#[cfg(unix)]
const EXEC_HANDOVER_CERT_REFUSED_LINE: &str = "clauth-exec-handover-cert-refused\n";

/// The probe's stdout cap: the bare token, or the cert-refused marker line
/// plus the reason the verdict logs (the sanitizer keeps 200 chars of it).
/// 4 KiB after the marker leaves every real reason whole while a run-away
/// writer stays bounded; the token comparison stays exact on the bytes the
/// cap let through.
#[cfg(unix)]
const EXEC_PROBE_OUTPUT_CAP: usize = EXEC_HANDOVER_CERT_REFUSED_LINE.len() + 4096;

/// What the capability probe answered.
#[cfg(unix)]
enum ReplacementVerdict {
    /// The exact token, exit 0: the replacement takes the handover.
    Capable,
    /// Anything else that is not a cert refusal — a build that predates the
    /// handover (it exits on the unknown argument), a hang, a spawn failure,
    /// another program at the path.
    NoHandover,
    /// The cert-refused marker line plus the reason: the replacement ran the
    /// same certificate read the exec'd start would, and it failed.
    CertRefused(String),
}

/// Probe the replacement at the recorded path for the restart-handover
/// capability, before the stop: spawn it with the re-exec's own command (the
/// path with this process's argv kept) plus the hidden flag, so it judges
/// the exact start the exec would run, in this process's env and cwd. Stdin
/// null, stdout read up to [`EXEC_PROBE_OUTPUT_CAP`], stderr drained and
/// dropped; [`crate::gateway::run_bounded`] kills and reaps a child that
/// outruns [`EXEC_PROBE_BOUND`]. A build that predates the handover exits on
/// the unknown argument and reads as no handover.
#[cfg(unix)]
fn probe_replacement(path: &std::path::Path) -> ReplacementVerdict {
    let mut command = reexec_command(path);
    command.arg(EXEC_HANDOVER_FLAG);
    match crate::gateway::run_bounded(
        &mut command,
        path,
        EXEC_PROBE_BOUND,
        Some(EXEC_PROBE_OUTPUT_CAP),
    ) {
        Ok(crate::gateway::Bounded::Exited(exited)) => {
            let answer = exited.stdout();
            if let Some(reason) = answer.strip_prefix(EXEC_HANDOVER_CERT_REFUSED_LINE.as_bytes()) {
                // The bytes come from whatever program sits at the path, so
                // the reason is sanitized where the line is rendered.
                ReplacementVerdict::CertRefused(String::from_utf8_lossy(reason).into_owned())
            } else if exited.status.success() && answer == EXEC_HANDOVER_TOKEN.as_bytes() {
                ReplacementVerdict::Capable
            } else {
                ReplacementVerdict::NoHandover
            }
        }
        _ => ReplacementVerdict::NoHandover,
    }
}

/// One re-exec evaluation: when due, run the stop (children + spend drain),
/// then, unless a stop signal was parked while the stop ran, the exec; an
/// exec that returned reaches the abort. A parked stop signal ends the run by
/// the signal-exit step instead: die of that signal, never exec. Each side
/// effect is injected so the order, the lost-race arm, the parked-signal arm
/// and the failure arm pin without stopping children, exec-ing, dying or
/// aborting the test process. The stop reports whether it ran: a stop that
/// found the kit already taken lost the race to the signal path, which then
/// owns this process's exit.
#[cfg(unix)]
fn reexec_step(
    due: bool,
    on_stop: impl FnOnce() -> bool,
    pending_signal: impl FnOnce() -> Option<libc::c_int>,
    on_exec: impl FnOnce() -> bool,
    on_signal_exit: impl FnOnce(libc::c_int),
    on_abort: impl FnOnce(),
) {
    if !due {
        return;
    }
    if !on_stop() {
        return;
    }
    if let Some(signal) = pending_signal() {
        on_signal_exit(signal);
        return;
    }
    if !on_exec() {
        on_abort();
    }
}

/// The production re-exec command: the recorded path with this process's own
/// argv kept (see [`reexec_command_with_argv`]) and the inherited env and cwd
/// the `Command` defaults carry.
#[cfg(unix)]
fn reexec_command(path: &std::path::Path) -> std::process::Command {
    reexec_command_with_argv(path, std::env::args_os())
}

/// The re-exec command built from the argv source it is given: argv[0]
/// included, because the daemon's identity guard reads argv[0]'s basename and
/// a symlinked start must survive the re-exec, the rest as the args. The
/// source is a seam — production passes the process's own argv, the test one
/// it controls, so a re-entered child runs a single `--exact` test instead of
/// re-running the harness's whole filter.
#[cfg(unix)]
fn reexec_command_with_argv(
    path: &std::path::Path,
    argv: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;
    let mut argv = argv.into_iter();
    // argv[0] is taken before the args: `args` consumes the rest, so a later
    // `next` would read past the end and the override would never fire.
    let argv0 = argv.next();
    let mut command = std::process::Command::new(path);
    command.args(argv);
    if let Some(argv0) = argv0 {
        command.arg0(argv0);
    }
    command
}

/// The re-exec wiring's test hooks: the exec attempt and the signal exit run
/// for real by default, but a test can arm a replacement (the real ones exec
/// or exit the test process) and count the production stop runs. Per-thread,
/// so parallel tests never see each other's arms; every wiring test resets
/// the slots first, because a test that arms a hook and never takes it (a red
/// one) would otherwise hand its arm to the next test on the same thread —
/// which only the threaded harness's `WouldBlock` fallback makes possible
/// (libtest spawns a thread per test, nextest a process per test).
#[cfg(all(test, unix))]
mod reexec_wiring_test {
    use std::cell::{Cell, RefCell};

    /// The armed exec attempt: runs in place of the real exec; `true` =
    /// success. It receives what the real exec would run: the recorded
    /// path and the command the production step built, the handover
    /// already applied to it.
    type ExecHook = Box<dyn FnOnce(&std::path::Path, std::process::Command) -> bool>;
    /// The armed signal exit: runs in place of the real re-raise + `exit`.
    type SignalExitHook = Box<dyn FnOnce(libc::c_int)>;
    /// The armed abort: runs in place of the real `std::process::abort`, so a
    /// test observes the abort arm without ending its process.
    type AbortHook = Box<dyn FnOnce()>;

    thread_local! {
        static EXEC: RefCell<Option<ExecHook>> = const { RefCell::new(None) };
        static SIGNAL_EXIT: RefCell<Option<SignalExitHook>> = const { RefCell::new(None) };
        static ABORT: RefCell<Option<AbortHook>> = const { RefCell::new(None) };
        static STOPS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn reset() {
        EXEC.with(|slot| *slot.borrow_mut() = None);
        SIGNAL_EXIT.with(|slot| *slot.borrow_mut() = None);
        ABORT.with(|slot| *slot.borrow_mut() = None);
        STOPS.with(|stops| stops.set(0));
    }

    /// Run in place of the real exec attempt while armed; `true` = success.
    pub(super) fn arm_exec(
        exec: impl FnOnce(&std::path::Path, std::process::Command) -> bool + 'static,
    ) {
        EXEC.with(|slot| *slot.borrow_mut() = Some(Box::new(exec)));
    }

    pub(super) fn take_exec() -> Option<ExecHook> {
        EXEC.with(|slot| slot.borrow_mut().take())
    }

    /// Run in place of the real signal exit (the re-raise + `exit`) while armed.
    pub(super) fn arm_signal_exit(exit: impl FnOnce(libc::c_int) + 'static) {
        SIGNAL_EXIT.with(|slot| *slot.borrow_mut() = Some(Box::new(exit)));
    }

    pub(super) fn take_signal_exit() -> Option<SignalExitHook> {
        SIGNAL_EXIT.with(|slot| slot.borrow_mut().take())
    }

    /// Run in place of the real abort while armed.
    pub(super) fn arm_abort(abort: impl FnOnce() + 'static) {
        ABORT.with(|slot| *slot.borrow_mut() = Some(Box::new(abort)));
    }

    pub(super) fn take_abort() -> Option<AbortHook> {
        ABORT.with(|slot| slot.borrow_mut().take())
    }

    /// Count the production stop runs the wiring observed.
    pub(super) fn observe_stop() {
        STOPS.with(|stops| stops.set(stops.get() + 1));
    }

    pub(super) fn stops() -> usize {
        STOPS.with(std::cell::Cell::get)
    }
}

/// Log the exec leg's failure and answer false, so [`reexec_step`] takes its
/// abort: a supervisor restarts the daemon fresh, the only safe end once the
/// stop has run and no image is serving.
#[cfg(unix)]
fn reexec_failed(path: &std::path::Path, error: impl std::fmt::Display) -> bool {
    logline!(
        "clauth daemon: re-exec of {} failed ({error}); aborting for a clean restart",
        path.display()
    );
    false
}

/// The daemon's own listener start, recorded for the cert-refused verdict's
/// manual-restart advice: the address and the explicit certificate pair it
/// was started with, so the advised `--replace` reloads the same certificate
/// source (the bare `--listen` form would fall back to the lego source).
#[cfg(unix)]
#[derive(Debug, Default)]
struct RestartAdvice {
    listen: Option<SocketAddr>,
    /// `--cert`/`--key` when the start carried them.
    explicit: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

#[cfg(unix)]
impl RestartAdvice {
    /// The advice for this daemon, read off its own start: the `--listen`
    /// address and, under an explicit `--cert`/`--key` pair, those paths.
    fn from_source(listen: Option<SocketAddr>, certs: &api::tls::CertSource) -> Self {
        let explicit = match certs {
            api::tls::CertSource::Lego => None,
            api::tls::CertSource::Explicit(paths) => Some((paths.cert.clone(), paths.key.clone())),
        };
        Self { listen, explicit }
    }
}

/// How many chars of a cert-refused reason the log line keeps, after
/// sanitizing: the bytes come from whatever program sits at the path, so the
/// line stays one line of bounded length. Unicode format characters (a bidi
/// override among them) still pass through: `char` exposes no general-category
/// check, and none is added as a dependency for a cosmetic gap.
#[cfg(unix)]
const REASON_MAX_CHARS: usize = 200;

/// One argument of the advised command, shell-quoted where it needs to be, so
/// an address or certificate path with a space or a shell metacharacter pastes
/// as the one argument it was. A plain one keeps the approved line's unquoted
/// rendering.
///
/// `shlex` refuses only a NUL byte, which cannot appear in a Unix path or
/// argument at all; such text is left as it is.
#[cfg(unix)]
fn shell_arg(text: &str) -> String {
    match shlex::try_quote(text) {
        Ok(quoted) => quoted.into_owned(),
        Err(_) => text.to_owned(),
    }
}

/// One `--cert`/`--key` member of the advised command: a path starting with
/// `-` takes clap's `--flag=value` form, because the space-separated form
/// reads it as the next flag and shell quoting cannot change that. Every other
/// path keeps the space-separated form the approved line already had.
#[cfg(unix)]
fn cert_path_arg(flag: &str, path: &std::path::Path) -> String {
    let rendered = path.display().to_string();
    if rendered.starts_with('-') {
        format!(" {flag}={}", shell_arg(&rendered))
    } else {
        format!(" {flag} {}", shell_arg(&rendered))
    }
}

/// The manual-restart command the cert-refused verdict advises: the daemon's
/// own `--listen` address and, when it started with an explicit pair, the
/// `--cert`/`--key` paths, each shell-quoted so it pastes whole; a path
/// starting with `-` takes the `--flag=value` form so clap reads it as the
/// value. A daemon that serves no API gets the command without a listener —
/// the only start that reproduces its own.
#[cfg(unix)]
fn restart_command(advice: &RestartAdvice) -> String {
    let mut command = String::from("clauth daemon --replace");
    if let Some(addr) = advice.listen {
        command.push_str(" --listen ");
        command.push_str(&shell_arg(&addr.to_string()));
    }
    if let Some((cert, key)) = &advice.explicit {
        command.push_str(&cert_path_arg("--cert", cert));
        command.push_str(&cert_path_arg("--key", key));
    }
    command
}

/// The cert-refused verdict's log line: the approved wording, the reason
/// sanitized to one line of at most [`REASON_MAX_CHARS`] chars (control
/// characters become spaces), and the manual-restart command from
/// [`restart_command`].
#[cfg(unix)]
fn cert_refused_line(path: &std::path::Path, reason: &str, advice: &RestartAdvice) -> String {
    let reason: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(REASON_MAX_CHARS)
        .collect();
    format!(
        "clauth daemon: {} cannot load the API certificate ({reason}); keeping this image \
         running until a manual restart: `{}` after fixing the certificate",
        path.display(),
        restart_command(advice),
    )
}

/// The run loop's per-tick re-exec check: feed the watch, and once it is due,
/// probe the replacement at the recorded path for the restart-handover
/// capability BEFORE anything stops — a build that cannot take the handover
/// must never be exec'd into, because its fresh claim would collide with the
/// inherited lock and the daemon would die as "already running". A
/// not-capable verdict and a cert refusal each log once and stand down: no
/// stop, no drain, no exec, and the watch remembers the binary's identity,
/// so the same file is never re-probed on later checks while a new
/// replacement re-arms it. A cert refusal is its own line — the replacement
/// cannot load the API certificate its start would serve — carrying the
/// manual-restart command `advice` names (this daemon's own `--listen`
/// address and explicit certificate pair, so the advised restart loads the
/// same source). A capable verdict restamps the heartbeat (the probe and
/// the stop's serial legs each get their own watchdog window), logs, stops
/// the children and drains the spend in flight through the signal path's
/// own [`gateway::stop_for_exit`], then execs the path recorded at start —
/// never `current_exe()`, which reads `<path> (deleted)` after a
/// replacement. A stop signal the watcher parked while the stop ran ends
/// the process by that signal instead, exactly as the signal path would.
/// An exec that returned failed — and a recorded path whose bytes changed
/// after the probe, so the exec would run an unprobed image — logs and
/// aborts so the unit restarts the daemon.
#[cfg(unix)]
#[expect(
    clippy::exit,
    reason = "a stop signal parked while the re-exec's stop ran ends the process by that signal instead of the exec, as the signal path does"
)]
fn reexec_if_due(
    watch: &mut ReexecWatch,
    kit: &gateway::StopKit,
    heartbeat: &AtomicU64,
    advice: &RestartAdvice,
) {
    use std::os::unix::process::CommandExt as _;
    if !watch.observe(crate::plugin_host::self_exe_change()) {
        return;
    }
    let Some(path) = crate::plugin_host::recorded_self_exe_path() else {
        return;
    };
    let identity = BinaryIdentity::of(&path);
    // A stood-down verdict on a path the watch cannot key stays standing: no
    // identity can tell that replacement from a later one, so probing every
    // due cycle would spawn and log once per cycle on a file it can never
    // vouch for. A keyable file at the path moves on to the comparison.
    if watch.stood_down() && identity.is_none() {
        watch.reset_streak();
        return;
    }
    // The replacement the last probe stood down on still sits at the path:
    // nothing re-checks, and the streak resets so a NEW replacement (a
    // different identity) re-arms from two consecutive reads.
    if watch
        .stood_down_on()
        .zip(identity)
        .is_some_and(|(stood, now)| stood == now)
    {
        watch.reset_streak();
        return;
    }
    match probe_replacement(&path) {
        ReplacementVerdict::NoHandover => {
            watch.stand_down(identity);
            logline!(
                "clauth daemon: {} does not understand the restart handover; keeping this image running until a manual restart",
                path.display()
            );
            return;
        }
        ReplacementVerdict::CertRefused(reason) => {
            watch.stand_down(identity);
            logline!("{}", cert_refused_line(&path, &reason, advice));
            return;
        }
        ReplacementVerdict::Capable => {}
    }
    // The probe and the stop's serial legs cannot share one watchdog window
    // (EXEC_PROBE_BOUND + TICK + SPEND_DRAIN_BOUND + DAEMON_STOP_BUDGET +
    // STOP_ANSWER_GRACE exceeds WATCHDOG_DEADLINE): restamp the heartbeat
    // here, so each bounded leg gets its own window and a legal drain is
    // never cut mid-save.
    heartbeat.store(crate::usage::now_ms(), Ordering::Relaxed);
    logline!(
        "clauth daemon: its binary was replaced; re-executing {}",
        path.display()
    );
    reexec_step(
        true,
        || {
            let Some((spends, gateway, proxies)) = kit.take() else {
                // The kit is empty: the signal path took it first and owns
                // the end of this process, and re-exec-ing now would cut its
                // drain short. Unless the watcher never started (its spawn
                // failed at boot): then no stop owner exists, the children
                // were already stopped by that arm's drop, and the due
                // re-exec waits the token spend in flight out through the
                // spare drain the kit kept before it execs.
                if !kit.watcher_lost() {
                    return false;
                }
                let Some(spends) = kit.take_spare_drain() else {
                    return false;
                };
                #[cfg(all(test, unix))]
                reexec_wiring_test::observe_stop();
                // No child is left to stop, so the drain runs alone: a stop
                // thread for an idle supervision is a needless spawn through
                // a thread limit that already lost the watcher once.
                spends.drain(SPEND_DRAIN_BOUND);
                return true;
            };
            #[cfg(all(test, unix))]
            reexec_wiring_test::observe_stop();
            gateway::stop_for_exit(&spends, gateway, proxies);
            true
        },
        || kit.take_pending_stop_signal(),
        || {
            // The capable verdict is bound to the bytes the probe spawned: a
            // second replacement can land while the stop runs (up to the
            // whole drain bound), and exec'ing it unprobed is the death the
            // probe exists to prevent. The stop already ran, so a stand-down
            // can no longer leave a serving daemon: the exec-failure abort
            // restarts the unit fresh.
            if BinaryIdentity::of(&path) != identity {
                return reexec_failed(&path, "the replacement changed after the probe");
            }
            // Hand the singleton's descriptors over first: the exec closes
            // every close-on-exec fd std opens, so without this the flock
            // and the listener would drop here and reopen the gap to a
            // parked standby. The pair is named on the exec command's env,
            // never this process's (which the live threads must not race).
            // The command is built and the handover applied before the test
            // hook below, so a test observes exactly the command the real
            // exec runs.
            let handover = exec_handover::prepare_for_exec();
            let mut command = reexec_command(&path);
            exec_handover::apply_handover(&mut command, handover);
            #[cfg(all(test, unix))]
            if let Some(exec) = reexec_wiring_test::take_exec() {
                return exec(&path, command);
            }
            // `exec` returns only its error: on success the image is replaced
            // and this process never comes back.
            let e = command.exec();
            reexec_failed(&path, e)
        },
        |signal| {
            #[cfg(all(test, unix))]
            if let Some(signal_exit) = reexec_wiring_test::take_signal_exit() {
                signal_exit(signal);
                return;
            }
            // Die of the parked stop signal exactly as the signal path does:
            // the default disposition, then the conventional exit code.
            if let Err(e) = signal_hook::low_level::emulate_default_handler(signal) {
                logline!("clauth daemon: cannot re-raise signal {signal}: {e}");
            }
            std::process::exit(128 + signal);
        },
        || {
            #[cfg(all(test, unix))]
            if let Some(abort) = reexec_wiring_test::take_abort() {
                abort();
                return;
            }
            std::process::abort()
        },
    );
}

/// The per-tick check on platforms that never re-exec: the watch is fed like
/// the unix side so the loop keeps one shape, and it never returns true here
/// (a console daemon has no exec and no stop signal).
#[cfg(not(unix))]
fn reexec_if_due(watch: &mut ReexecWatch, _kit: &gateway::StopKit, _heartbeat: &AtomicU64) {
    watch.observe(crate::plugin_host::self_exe_change());
}

/// Tighten an existing `~/.clauth` tree on boot, before `load_config` runs its
/// own walk. `mkdir_700` only sets the mode on dirs it CREATES; a tree from an
/// older build or created under a permissive umask can be 0o755
/// (world-traversable → world-readable `daemon.log`, enumerable account names).
/// Delegates to [`crate::profile::enforce_clauth_perms`] for the whole tree
/// short of each codex home's contents (dirs → 0o700, files → 0o600, or 0o700
/// when the file's owner exec bit is set, symlinks skipped), which also covers
/// the launchd-created `daemon.log`: launchd opens it (`StandardErrorPath`) at
/// the umask (~0o644) before `exec`, and the already-open fd keeps appending to
/// the now-0o600 inode. Best-effort — a chmod failure never stops the daemon.
fn migrate_clauth_perms_700(dir: &std::path::Path) {
    crate::profile::enforce_clauth_perms(dir);
}

/// What a starting `clauth daemon` does when another instance already holds the
/// singleton lock (#57).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StartMode {
    /// The default (and the `--no-standby` spelling). Exit 0 the moment the lock
    /// is lost: a daemon is already running, which is the desired end state. A
    /// pure supervisor never reaches this — it wins the boot race alone and
    /// `KeepAlive{SuccessfulExit=false}` restarts it on crash.
    ExitIfRunning,
    /// `--standby`. Park in the one standby slot and take over the moment the
    /// holder exits. The launchd/systemd-paired-with-a-manual-run mix is the
    /// only setup that needs it: a supervisor's instance has to queue behind a
    /// manually run one it could never be restarted behind after a clean exit.
    Standby,
    /// `--replace`. Terminate the running daemon, wait for its flock to release
    /// on death, then take over. For an in-place upgrade, where the operator
    /// wants the new binary running now rather than on the next restart.
    Replace,
}

/// Opt-out for the REST API, matching `CLAUTH_NO_UPDATE` / `CLAUTH_NO_COMPLETIONS`
/// (only `"1"` opts out). A listening socket is the one daemon behavior an
/// operator might need to kill without editing the unit that passes `--listen`.
const NO_API_ENV: &str = "CLAUTH_NO_API";

fn api_enabled() -> bool {
    std::env::var(NO_API_ENV).as_deref() != Ok("1")
}

/// The listener decision `serve` runs at start, and the handover probe's
/// answer runs identically (`main.rs`), so the probe can never judge a
/// different start than the exec runs. Extracted because it is the REST kill
/// switch's call site: `Some(addr)` under `CLAUTH_NO_API=1` must yield
/// `no_api`, never a prepared listener. The prepare arm only reads the
/// certificate — the first-use `tls.json` is `serve`'s write above it — and
/// the legacy import and the bind itself run later, below the claim, in
/// `api::serve_prepared`.
pub(crate) fn listener_setup(
    listen: Option<SocketAddr>,
    certs: &api::tls::CertSource,
) -> Result<(Option<api::Prepared>, Option<SocketAddr>)> {
    Ok(match listen {
        Some(addr) if api_enabled() => (Some(api::prepare(addr, certs)?), None),
        Some(addr) => (None, Some(addr)),
        None => (None, None),
    })
}

/// The TUI's `start daemon`: `<exe> daemon` detached from the caller, its
/// output appended to [`LOG_FILE`] (append mode, which the size cap's
/// in-place trim needs), its cwd `~/.clauth` so it pins no directory the
/// caller ran in. On unix it leads its own process group, so the caller's
/// Ctrl-C and the terminal's hangup, which reach only the foreground group,
/// never reach it; on Windows it runs on a hidden console of its own, so
/// closing the caller's console window does not end it, the console programs
/// it starts (the gateway, its PowerShell probes) share that hidden console
/// instead of each opening a window, and it leaves the caller's job object
/// where that job allows it, so a host closing the job (sshd ending a
/// session) does not end it either. A clauth session home the caller inherited
/// is scrubbed, with the operator's own codex home put back in a session
/// `CODEX_HOME`'s place ([`crate::runtime::scrub_clauth_homes`]): the daemon
/// outlives that session and its tree.
pub(crate) fn spawn_detached(exe: &std::path::Path) -> Result<std::process::Child> {
    let dir = clauth_dir()?;
    mkdir_700(&dir).context("failed to create ~/.clauth")?;
    let log_path = dir.join(LOG_FILE);
    let log = crate::profile::open_append_600(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().context("failed to share the daemon log")?)
        .stderr(log);
    crate::runtime::scrub_clauth_homes(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Process creation flags (winbase.h): a console with no window, its
        // own Ctrl+C group, and out of the caller's job object, which a host
        // such as sshd closes with every process in it. A job that forbids
        // breakaway refuses the whole spawn, so that one retries inside it.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const ERROR_ACCESS_DENIED: i32 = 5;
        let detached = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        command.creation_flags(detached | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                command.creation_flags(detached);
            }
            spawned => {
                return spawned.with_context(|| format!("failed to run {} daemon", exe.display()));
            }
        }
    }
    command
        .spawn()
        .with_context(|| format!("failed to run {} daemon", exe.display()))
}

/// How long the TUI's `start daemon` waits for its child to hold the
/// singleton. `serve` claims it before any shared-tree work, so this is
/// headroom, not an expected duration.
pub(crate) const START_WAIT: Duration = Duration::from_secs(5);

/// What a [`spawn_detached`] child did within the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartOutcome {
    /// A daemon holds the singleton: this child, or one that won the race to
    /// it (this child then exits as redundant, the same end state).
    Holding,
    /// The child exited and nothing holds the singleton.
    Exited,
    /// Neither, when the wait ran out.
    NotYet,
}

/// Watch a [`spawn_detached`] child until a daemon holds the singleton, the
/// child exits, or `wait` passes. An unreadable lock counts as not held yet.
pub(crate) fn await_start(
    child: &mut std::process::Child,
    wait: Duration,
    poll: Duration,
) -> StartOutcome {
    probe::poll_until(wait, poll, || {
        if singleton_held().unwrap_or(false) {
            Some(StartOutcome::Holding)
        } else {
            matches!(child.try_wait(), Ok(Some(_))).then_some(StartOutcome::Exited)
        }
    })
    .unwrap_or(StartOutcome::NotYet)
}

/// How long a stopping daemon waits for the token spend in flight to save
/// before it exits anyway: one token call's end-to-end bound,
/// [`TOKEN_HTTP_DEADLINES`](crate::oauth::TOKEN_HTTP_DEADLINES), plus a second
/// for the stage write that makes the minted pair durable after the endpoint
/// answers. Derived from that bound, never a literal, so retuning the client
/// moves the exit wait with it. The re-exec path runs this wait on the main
/// loop, so it must stay inside [`WATCHDOG_DEADLINE`] with a [`TICK`] of
/// budget — the watchdog's own slow-tick allowance — or a long drain aborts
/// the daemon mid-save.
pub(crate) const SPEND_DRAIN_BOUND: Duration =
    crate::oauth::TOKEN_HTTP_DEADLINES.saturating_add(Duration::from_secs(1));

/// The scheduler's tick thread as a clean stop sees it: what
/// `usage::spawn_refresher` returns. The thread saves the tps producer's
/// unsaved attribution (applied proxy token records included) as its last act,
/// and the producer lives on that thread alone, so a stop wakes it and waits
/// for it to end.
#[derive(Clone, Debug)]
pub(crate) struct TickThread {
    /// `None` where no thread was spawned (a test build's skip). The join
    /// handle sits in a shared slot so a finished thread can be taken out and
    /// joined: the join is the only reader of a panic, which ends the thread
    /// without running its exit save.
    thread: Option<Arc<Mutex<Option<std::thread::JoinHandle<()>>>>>,
    shutting_down: Arc<AtomicBool>,
}

impl TickThread {
    pub(crate) fn new(
        thread: Option<std::thread::JoinHandle<()>>,
        shutting_down: Arc<AtomicBool>,
    ) -> Self {
        Self {
            thread: thread.map(|handle| Arc::new(Mutex::new(Some(handle)))),
            shutting_down,
        }
    }

    /// Raise the stop flag and cut the thread's sleep short. Returns at once:
    /// a tick in flight finishes first, then the thread saves and ends.
    pub(crate) fn stop(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(thread) = &self.thread
            && let Ok(guard) = thread.lock()
            && let Some(handle) = guard.as_ref()
        {
            handle.thread().unpark();
        }
    }

    /// Wait until the thread ended, or `bound` past `started` — `true` when
    /// it ended. A tick still running then is abandoned with its unsaved
    /// records, as a crash would lose them, and says so once. A panic ends
    /// the thread but skips the exit save; the join reads it and says so
    /// once.
    pub(crate) fn wait_stopped(&self, started: Instant, bound: Duration) -> bool {
        let Some(thread) = &self.thread else {
            return true;
        };
        while !thread
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(std::thread::JoinHandle::is_finished))
            .unwrap_or(true)
        {
            let remaining = bound.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                logline!(
                    "clauth: the usage tick was still running after {} s; exiting without its last tps save",
                    bound.as_secs()
                );
                return false;
            }
            std::thread::sleep(gateway::stop_sleep(remaining));
        }
        if let Ok(mut guard) = thread.lock()
            && let Some(handle) = guard.take()
        {
            drop(guard);
            if handle.join().is_err() {
                logline!(
                    "clauth: the usage tick thread panicked; exiting without its last tps save"
                );
            }
        }
        true
    }
}

/// The daemon's stop gate for a token spend: the store the scheduler claims
/// spends in and the flag that refuses new ones, plus the tick thread whose
/// exit save the stop waits for. [`serve`] builds it once and every signal
/// watcher carries it, the `(Err, Err)` start arm included, so a stop signal
/// waits the spend in flight out before the process exits. What it does not
/// cover: a stop that lands before that watcher is installed — the scheduler
/// starts in `boot()`, and until the watcher exists a SIGTERM keeps the
/// default action, so the process dies and the span is lost like a crash —
/// and a process left with no watcher at all because signal registration
/// failed. Clone is a second handle to the same gate: the stop kit keeps one
/// as its spare for the watcher-lost re-exec.
#[derive(Clone)]
pub(crate) struct SpendDrain {
    #[cfg(unix)]
    activity: ActivityStore,
    #[cfg(unix)]
    shutting_down: Arc<AtomicBool>,
    #[cfg(unix)]
    scheduler: Option<TickThread>,
}

impl SpendDrain {
    pub(crate) fn new(activity: ActivityStore, shutting_down: Arc<AtomicBool>) -> Self {
        #[cfg(not(unix))]
        let _ = (activity, shutting_down);
        Self {
            #[cfg(unix)]
            activity,
            #[cfg(unix)]
            shutting_down,
            #[cfg(unix)]
            scheduler: None,
        }
    }

    /// The same gate, also stopping `scheduler` and waiting for its exit save.
    pub(crate) fn with_scheduler(self, scheduler: TickThread) -> Self {
        #[cfg(not(unix))]
        let _ = scheduler;
        Self {
            #[cfg(unix)]
            activity: self.activity,
            #[cfg(unix)]
            shutting_down: self.shutting_down,
            #[cfg(unix)]
            scheduler: Some(scheduler),
        }
    }

    /// Refuse every scheduler token spend that has not started and wake the
    /// tick thread, then wait until the spend already in flight saves or
    /// `bound` elapses — `true` when it cleared — and then for the tick
    /// thread's exit save. The flag goes up FIRST: a spend claiming the gap
    /// before a check could otherwise run past the drain and die unsaved, so
    /// the claim re-tests the flag under the store's lock. One log line when
    /// the spend wait actually waited, one when it gives up, none when nothing
    /// ran.
    ///
    /// The tick thread gets until `bound` past the drain's start — the same
    /// window as the spend wait, so the two waits never add. The drain ends
    /// within `SPEND_DRAIN_BOUND` (20 s) whatever the tick does, and
    /// `REPLACE_WAIT` (that plus 5 s) still lets the stop finish before
    /// `--replace` force-kills it. A clean stop loses the unsaved records
    /// only when the last tick outruns that window.
    #[cfg(unix)]
    pub(crate) fn drain(&self, bound: Duration) -> bool {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(scheduler) = &self.scheduler {
            scheduler.stop();
        }
        let started = Instant::now();
        let cleared = self.wait_spend(started, bound);
        if let Some(scheduler) = &self.scheduler {
            scheduler.wait_stopped(started, bound);
        }
        cleared
    }

    /// The spend half of [`Self::drain`], its flag already raised.
    #[cfg(unix)]
    fn wait_spend(&self, started: Instant, bound: Duration) -> bool {
        if !crate::usage::token_spend_in_flight(&self.activity) {
            return true;
        }
        match crate::usage::spending_account(&self.activity) {
            Some(account) => logline!(
                "clauth daemon: waiting up to {} s for the token save of '{account}' before exiting",
                bound.as_secs()
            ),
            None => logline!(
                "clauth daemon: waiting up to {} s for a token save before exiting",
                bound.as_secs()
            ),
        }
        while crate::usage::token_spend_in_flight(&self.activity) {
            let remaining = bound.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                logline!(
                    "clauth daemon: a token save was still running after {} s; exiting anyway",
                    bound.as_secs()
                );
                return false;
            }
            std::thread::sleep(gateway::stop_sleep(remaining));
        }
        true
    }
}

/// The singleton claim for a start that adopted nothing across the exec
/// (which is every start on platforms that never re-exec): `--replace`
/// terminates the incumbent and takes over, `--standby` parks a lone
/// waiter, and the default yields as redundant — the outcomes [`serve`]
/// matches on.
fn fresh_claim(dir: &std::path::Path, mode: StartMode) -> Result<Claim> {
    Ok(match mode {
        // A start whose pid sidecar names this process must never signal
        // it: after a refused handover the inherited lock descriptor stays
        // open (and its flock held), and the sidecar the replaced image
        // stamped still names this pid (the exec keeps it), so
        // `claim_by_replacing` would terminate its own pid. A plain claim
        // instead reads the singleton as held — or takes it, if the
        // descriptor is gone — and never signals itself.
        StartMode::Replace if probe::holder_pid() == Some(std::process::id()) => {
            claim_singleton(dir, false)?
        }
        StartMode::Replace => probe::claim_by_replacing(dir)?,
        _ => claim_singleton(dir, mode == StartMode::Standby)?,
    })
}

/// `clauth daemon` — build the shared stores, run the scheduler headless, and
/// loop executing auto-switches + rewriting `status.json` until killed.
///
/// `listen` is the REST API's bind address (`--listen`), or `None` for the
/// default file-only daemon. `certs` is where that listener's TLS identity
/// comes from, and is ignored without a `listen`.
pub(crate) fn serve(
    mode: StartMode,
    listen: Option<SocketAddr>,
    certs: &api::tls::CertSource,
) -> Result<()> {
    // First thing, before any output (including the standing-by line below):
    // daemon stderr IS daemon.log, and undated lines cost real forensics time
    // (2026-07-09 — see `logline`). First also so the capture's failure line
    // below is stamped like every other.
    crate::logline::enable_timestamps();
    crate::platform::init();

    let dir = clauth_dir()?;
    // Adopt the descriptors the replaced image handed over across the exec
    // before anything can spawn (the capture below spawns `ps` on macOS):
    // the handover env is removed here, so no child of this image inherits
    // it, and each fd is validated while borrowed. An adopted fd, and a
    // refused number its probe found open, is re-marked close-on-exec. A
    // refused descriptor is left open, logged with its fallback, and the
    // start falls back to the fresh claim and bind below.
    #[cfg(unix)]
    let handed_over_lock = exec_handover::take_at_start(&dir, listen);
    // The boot capture runs before anything can spawn: the children this
    // process inherited across a re-exec (the exec keeps the pid, so the
    // children the replaced image spawned are this image's too) are listed
    // while none of this image's own exist yet, so the run loop's per-tick
    // reap can never take a `Child` this image owns. A fresh start captures
    // an empty set.
    gateway::capture_inherited_children();
    // Create ~/.clauth at 0o700 (was create_dir_all → umask 0o755). Above the
    // singleton claim because the lock files live in the dir; it no-ops for
    // every instance after the first.
    mkdir_700(&dir).context("failed to create ~/.clauth")?;

    // The listener's unreadable certificate is settled BEFORE the claim below,
    // because the claim is what terminates the incumbent under `--replace`.
    // Failing after it would leave the host with no daemon at all: no refresh,
    // no auto-switch, not merely no listener. Nothing else settles here: the
    // legacy import and the bind deliberately do NOT run above the claim — both
    // live in `api::serve_prepared`, below it (and below a standby's
    // promotion), where the incumbent's port is free, a redundant instance
    // never reaches them, and a start that dies cannot have written
    // `devices.json` or deleted `auth_token.json`.
    //
    // The first-use `tls.json` is this start's write, never the handover
    // probe's answer: the probe runs the same listener decision below and must
    // leave the home alone. `listener_setup` branches on a listener and the
    // REST kill switch; the lego source below is this write's own condition,
    // since only the lego read inside `api::prepare` consults the file — an
    // explicit `--cert`/`--key` pair never does.
    if listen.is_some() && api_enabled() && matches!(certs, api::tls::CertSource::Lego) {
        api::tls::create_default_tls_config()?;
    }
    let (mut prepared, no_api) = listener_setup(listen, certs)?;

    // Single-instance guard, claimed BEFORE any shared-tree work below: a
    // redundant instance must not GC the live daemon's runtime forest or walk
    // its modes. The default exits the moment the lock is lost; only `--standby`
    // parks a lone waiter so a supervisor's instance can take over from a
    // manually run one (a clean exit is never restarted under launchd
    // `KeepAlive{SuccessfulExit=false}`); `--replace` terminates the holder and
    // takes over (#57). A dead holder's advisory flock auto-releases, so neither
    // a standby nor a replace is ever orphaned. A re-exec'd image that adopted
    // the handed-over lock skips the claim entirely: it already holds the
    // singleton, and there must be no instant of the restart where a parked
    // `--standby` could take it.
    #[cfg(unix)]
    let claim = match handed_over_lock {
        // The replaced image's lock, adopted: a fresh claim would only lose
        // to its own flock, and `--replace` must not terminate its own pid.
        Some(file) => Claim::Active(probe::DaemonLock::adopt_handed_over(file)),
        None => fresh_claim(&dir, mode)?,
    };
    #[cfg(not(unix))]
    let claim = fresh_claim(&dir, mode)?;
    let (lock, promoted) = match claim {
        Claim::Active(lock) => (lock, false),
        Claim::Standby(slot) => (stand_by(&dir, slot)?, true),
        Claim::Redundant => {
            logline!("clauth daemon: {}; exiting", redundant_reason(mode));
            return Ok(());
        }
    };

    // A standby carried its pre-claim certificate through a park that is
    // unbounded by design, so the promoted daemon re-reads it here: a renewal
    // that landed during the park is what the listener serves, and a
    // replacement that no longer reads fails the start fatally — the operator
    // asked for a listener — rather than leaving a healthy-looking daemon on
    // the stale identity. Every other start read the certificate moments ago
    // and skips this.
    if promoted && let Some(prepared) = prepared.as_mut() {
        prepared.reload_certificate(certs)?;
        logline!("clauth daemon: standby promoted; TLS certificate reloaded");
    }

    // Record the env this daemon inherited, once it is definitively the holder,
    // so the store move resolves the standalone's codex home and store defaults
    // the way the gateway it spawns will. Best-effort: a daemon that cannot
    // write it still runs, and the reads of it refuse until a later
    // gateway round records it.
    if let Err(e) = crate::gateway::write_daemon_env() {
        logline!(
            "clauth daemon: {e:#}{}",
            crate::gateway::ENV_RECORD_FAULT_TAIL
        );
    }

    log_rotate::warn_if_log_cap_defeated();
    // Tighten an existing looser tree (older builds / CLI umask left it 0o755)
    // before `load_config` runs its own walk. Idempotent, so the standby path
    // running it twice costs one stat walk.
    migrate_clauth_perms_700(&dir);
    crate::runtime::gc_stale_runtimes();

    let config = load_config()?;
    warn_if_spend_is_uncapped(&config);
    warn_if_chain_has_no_usage_source(&config);
    let mut daemon = Daemon::new(config, dir.join(STATUS_FILE));
    #[cfg(unix)]
    {
        daemon.restart_advice = RestartAdvice::from_source(listen, certs);
    }
    daemon.boot();

    // After `boot` (the stores are seeded and the scheduler is up, so a request
    // arriving immediately gets real numbers) and before `run` (which never
    // returns). The certificate was settled by `api::prepare` above the claim
    // (a promoted standby re-read it right after its promotion); the legacy import
    // and the bind happen here for the first time, on a port that is winnable
    // exactly now: the incumbent under `--replace` is dead, and a promoted
    // standby holds the claim it parked for.
    if let Some(addr) = no_api {
        // Said here rather than above the claim so a redundant instance cannot
        // print it and then "already running": two lines from a process that
        // did nothing.
        logline!("clauth daemon: {NO_API_ENV}=1 is set; not serving the REST API on {addr}");
    }
    if let Some(prepared) = prepared {
        let ctx = match api::serve_prepared(
            prepared,
            Arc::clone(&daemon.config),
            daemon.status_path.clone(),
            daemon.live_stores(),
            daemon.memory_stores(),
        ) {
            Ok(ctx) => ctx,
            Err(e) => {
                // `boot` started the tick thread before this last start step
                // that can fail; stop it and wait for its exit save, as the
                // signal path would, or the records applied since the last
                // save are lost like a crash.
                if let Some(scheduler) = daemon.scheduler.get() {
                    scheduler.stop();
                    let _ = scheduler.wait_stopped(Instant::now(), SPEND_DRAIN_BOUND);
                }
                return Err(e);
            }
        };
        daemon.push = api::start_push(&ctx);
    }

    // After the listener, the last start step that can fail, so a start that
    // dies leaves no child behind; before `run`, which never returns, so the
    // supervisors, held by the stop kit the signal watcher and the re-exec
    // path share, live as long as the process. A standby reaches this only
    // once promoted: the gateway and the proxies run under the singleton's
    // holder alone.
    let gateway = gateway::start(Arc::clone(&daemon.gateway), &lock);
    let proxies = proxies::start(Arc::clone(&daemon.proxies), &lock);
    // Every arm installs the watcher, the both-failed one included: with no
    // child to stop the daemon still owes the token spend in flight its save.
    let spends = daemon.spend_drain();
    let stop_kit = match (gateway, proxies) {
        (Ok(gateway), Ok(proxies)) => gateway::stop_on_signal(spends, Some(gateway), proxies),
        (Ok(gateway), Err(e)) => {
            logline!("clauth daemon: {e:#}; proxies are not supervised");
            gateway::stop_on_signal(spends, Some(gateway), proxies::ProxySupervision::idle())
        }
        (Err(e), Ok(proxies)) => {
            logline!("clauth daemon: {e:#}; the shunt gateway is not supervised");
            gateway::stop_on_signal(spends, None, proxies)
        }
        (Err(gateway), Err(proxy)) => {
            logline!("clauth daemon: {gateway:#}; the shunt gateway is not supervised");
            logline!("clauth daemon: {proxy:#}; proxies are not supervised");
            gateway::stop_on_signal(spends, None, proxies::ProxySupervision::idle())
        }
    };

    // Under the singleton's holder alone, like the supervisors above. Detached:
    // it lives as long as the process, and every write it makes is one atomic
    // rename under the state flock.
    let retention = crate::profile::claude_dir().and_then(|claude_home| {
        spawn_retention_watch(
            &claude_home,
            RETENTION_TIMINGS,
            crossbeam_channel::never(),
            retention_check,
        )
        .context("failed to start the cleanupPeriodDays check")
    });
    if let Err(e) = retention {
        logline!("clauth daemon: {e:#}");
    }

    logline!(
        "clauth daemon: running (status → {})",
        daemon.status_path.display()
    );
    daemon.run(&stop_kit);
    Ok(())
}

/// The [`Claim::Standby`] arm of [`serve`]: tighten the tree, say so, then park
/// until the holder exits. Extracted because the ORDER inside it is the whole
/// point and a park is unbounded in time, so nothing reachable through `serve`
/// can observe it.
///
/// The perms walk runs before the park rather than after the promotion: launchd
/// opens `StandardErrorPath` at the umask (0o644) BEFORE `exec`, so the
/// standing-by line would otherwise sit in a world-readable `daemon.log` naming
/// accounts for the whole wait.
fn stand_by(dir: &std::path::Path, slot: StandbySlot) -> Result<DaemonLock> {
    migrate_clauth_perms_700(dir);
    logline!("clauth daemon: another instance holds the lock: standing by until it exits");
    slot.promote()
}

/// Every chain member armed to spend with nothing to stop it (see
/// [`crate::fallback::spend_is_uncapped`]) — the pure collection
/// [`warn_if_spend_is_uncapped`] logs. Pulled out as its own fn so the filter
/// chain is testable without capturing log output.
///
/// A disabled member is excluded: it is never spend-armed by the walk
/// (the walk skips it as a candidate), so it can't be the uncapped
/// spender this names. Auth-broken and canceled members stay named even though
/// that same walk skips them too: both clear on their own (a re-login, a
/// re-subscribe), so going quiet about a member one re-auth away from billing
/// errs the wrong way. `spend_is_uncapped` excludes them for the opposite
/// reason: there they would count as a SINK catching the spend, where a
/// hopeful read invents a safety net that isn't there.
fn uncapped_spenders(config: &crate::profile::AppConfig) -> Vec<&str> {
    // Boot-time one-shot with no refresher's live set: the durable verdict is
    // the only key-rejected source here, read once (never per member).
    let key_rejected = crate::fallback::durable_key_rejected(config);
    config
        .state
        .fallback_chain
        .iter()
        .filter_map(|name| config.find(name))
        .filter(|p| !p.is_disabled())
        .filter(|p| {
            crate::fallback::spend_is_uncapped(
                config,
                p.max_auto_spend.unwrap_or(0.0),
                &key_rejected,
                // Boot-time one-shot with no live store yet: the picked disk
                // reading is the canceled source, the one the TUI card reads. A
                // third-party-classified sink's picked reading is its provider
                // stats, which carry no plan, so its OAuth plan never cancels it.
                |n| {
                    config.find(n).is_some_and(|p| {
                        !p.usage_cache_is_third_party()
                            && crate::profile_json::refuses_service_cached(p)
                    })
                },
            )
        })
        .map(|p| p.name.as_str())
        .collect()
}

/// Say so at boot when a chain member is armed to spend with nothing to stop it:
/// billing enabled is the operator's to know, but "the ceiling you set only
/// gates when spending STARTS" is not something a headless run would ever
/// discover. The TUI warns on the member card; nobody is watching that here.
///
/// Names each member rather than counting them — the operator has to know which
/// account to go fix.
fn warn_if_spend_is_uncapped(config: &crate::profile::AppConfig) {
    let uncapped = uncapped_spenders(config);
    if !uncapped.is_empty() {
        logline!(
            "clauth daemon: {} can spend with no cap. {}. without one, max spend only gates when \
             billing starts, not when it stops",
            uncapped.join(", "),
            crate::fallback::uncapped_spend_fix(),
        );
    }
}

/// Every enabled chain member clauth cannot measure (see
/// [`crate::fallback::no_usage_source`]): the pure collection
/// [`warn_if_chain_has_no_usage_source`] logs, split out like
/// [`uncapped_spenders`] so the filter is testable without capturing log
/// output. A disabled member is left out: the walk never switches to it.
fn no_usage_source_members(config: &crate::profile::AppConfig) -> Vec<&str> {
    config
        .state
        .fallback_chain
        .iter()
        .filter_map(|name| config.find(name))
        .filter(|p| !p.is_disabled())
        // The sidecar read a render path must not make per frame costs nothing
        // here: this runs once at boot against an owned `AppConfig`
        // (`load_config`), so no config guard is held while it reads the files.
        .filter(|p| crate::fallback::no_usage_source(p, crate::claude::has_session_token(&p.name)))
        .map(|p| p.name.as_str())
        .collect()
}

/// Say so at boot when the chain holds a member clauth cannot measure: once the
/// chain lands there it stays ([`crate::fallback::no_usage_hazard`]). The TUI
/// says it when the member is added and on its card; a headless run, or a chain
/// edited by hand in `profiles.toml`, hears it here at the next daemon start (a
/// reload of the file stays quiet, like the uncapped-spend warning).
fn warn_if_chain_has_no_usage_source(config: &crate::profile::AppConfig) {
    let members = no_usage_source_members(config);
    if !members.is_empty() {
        // A daemon can boot with such a member ALREADY active (the chain walked
        // onto it before this process started, or the operator put it there),
        // and then the hazard is the state of the machine, not a forecast.
        //
        // With ONE member the lead is the one the card and the confirm share
        // ([`crate::fallback::no_usage_hazard_lead`]), so a single state cannot
        // wear three tenses across three surfaces; the sentence before it
        // already named the member. A LIST keeps this line's own lead because
        // `it` cannot name which of several, and the present-tense arm has to
        // say which one is active.
        let active = config.state.active_profile.as_ref();
        let lead = match members.as_slice() {
            [only] => {
                crate::fallback::no_usage_hazard_lead(active.is_some_and(|a| a.as_str() == *only))
                    .to_string()
            }
            _ => match active {
                Some(a) if members.contains(&a.as_str()) => format!("'{a}' is active now, and "),
                _ => "once one is active, ".to_string(),
            },
        };
        logline!(
            "clauth daemon: can't read the usage of {}. {}{}. {}",
            members.join(", "),
            lead,
            crate::fallback::no_usage_hazard(),
            // Spelled out, not `<name>`: this line already named them, and the
            // operator can paste what it prints. Same copy the member card
            // carries.
            crate::fallback::no_usage_fix(&members),
        );
    }
}

/// Why this instance has nothing to do, worded so the operator can tell a full
/// queue apart from the default's "one is already up". The default names the
/// holder's pid so a `ps` dump ties back to a line here; `--standby` reaches
/// this only when the slot is already taken. `--replace` never reaches it (it
/// terminates the holder and claims, or errors), so its arm is defensive.
fn redundant_reason(mode: StartMode) -> String {
    match mode {
        StartMode::ExitIfRunning => {
            let pid = probe::holder_pid().map_or_else(|| "unknown".to_string(), |p| p.to_string());
            format!("already running (pid {pid})")
        }
        StartMode::Standby => "a daemon and its standby are already running".to_string(),
        StartMode::Replace => "another instance already holds the lock".to_string(),
    }
}

/// `clauth daemon --status` — presence probe for a supervisor or a menu-bar app,
/// so "is one already up?" costs a try-lock instead of a spawn. One line on
/// stdout while a daemon is up (exit 0); exit 1 with nothing on stdout when
/// none is, matching the sessions surface's convention.
pub(crate) fn status_probe() -> Result<()> {
    // The presence DECISION goes through `singleton_held`, not the header chip's
    // `daemon_health`: the chip maps an unusable lock to `Absent`, which dims it
    // rather than asserting a daemon that may not be there, and a `--status ||
    // spawn` supervisor reading that as "none running" respawns forever on a
    // filesystem without working locks. Here the same condition is an error the
    // caller sees. `daemon_health` still owns the freshness word below.
    if !probe::singleton_held()? {
        anyhow::bail!("no clauth daemon is running");
    }
    let pid = probe::holder_pid().map_or_else(|| "unknown".to_string(), |p| p.to_string());
    let feed = if daemon_health() == DaemonHealth::Fresh {
        "fresh"
    } else {
        "stale"
    };
    let standby = if probe::standby_waiting() {
        ", standby waiting"
    } else {
        ""
    };
    outln!("running (pid {pid}, feed {feed}{standby})");
    Ok(())
}

/// `clauth status --json [--all|--disabled]` — single-shot serializer. Reads
/// the on-disk caches and prints the same shape the daemon writes, then
/// exits. No scheduler; freshness and next-refresh are derived from each
/// account's cache. `include_disabled` mirrors `build_status`'s flag of the same name
/// (hidden by default; `dispatch`'s `--all`/`--disabled` flips it).
pub(crate) fn status_oneshot(include_disabled: bool) -> Result<()> {
    let config = load_config()?;
    let interval = config.state.refresh_interval_ms;
    let body = build_status(&config, interval, None, include_disabled);
    outln!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// Republish `~/.clauth/status.json` from a process that is not the daemon, so a
/// switch landing in the TUI, in `clauth <name>`, or through the MCP tool reaches
/// the feed's readers at once.
///
/// Without this the feed is only ever written by a running daemon, which learns
/// of such a switch from the `profiles.toml` mtime on its next tick — and never
/// learns of it at all when no daemon is running, leaving the published file
/// naming an account the operator switched away from however long ago. That is
/// the one field an external reader (`clauth-tray`, a status bar) most needs to
/// be right, so a switch publishes it itself.
///
/// A live daemon OWNS the file: it republishes every tick with the scheduler's
/// in-memory signals (`fetch_status`, `next_refresh_at`, `pending_switch`) that
/// a single-shot build cannot see, and its own auto-switches already end in a
/// `write_status`. So this defers whenever the singleton lock is positively
/// held — that daemon's next tick, at most a second out, publishes the richer
/// body over the same active account. Only an uncertain probe (a filesystem
/// without working locks) publishes anyway: writing a slightly thinner body is
/// cheaper than leaving a stale one.
///
/// Best-effort by design. The switch has already landed and been persisted by
/// the time this runs; the caller must not fail because a status file could not
/// be written.
///
/// The stamp is the daemon's, never this publish's: `generated_at` is how every
/// reader (`clauth-tray`, the TUI's daemon chip) decides a daemon is alive, so
/// the republish carries the daemon's last stamp forward, or the epoch when no
/// daemon has ever published — see [`prior_generated_at`].
///
/// The daemon-presence probe stays unserialized. A daemon's first tick can land
/// between the probe and this publish, costing one stale stamp at worst: the next
/// tick replaces it. The switch-side body construction below remains outside the
/// state flock; only its publication guards and atomic commit use the brief
/// final hold.
///
/// Call it OUTSIDE the switch's `with_state_lock`, the way every caller in
/// `actions` does: [`build_status`] stats and reads each profile's caches and
/// sweeps the session flocks, and that disk work has no business extending the
/// critical section every other clauth process is queued behind.
///
/// Takes the shared [`ConfigHandle`] and snapshots the config the same way the
/// daemon's own writer and the API's switch do — the guard acquired for the
/// clone and released before any disk work, so the snapshot itself never holds
/// the config mutex across the build either.
pub(crate) fn publish_status(config: &crate::profile::ConfigHandle) {
    publish_status_with(config, || {});
}

/// [`publish_status`] with a hook between the status body's construction and
/// its commit — the window a competing publisher can land in. Production passes
/// a no-op; the regression tests use it to order two real publishers.
pub(crate) fn publish_status_with(
    config: &crate::profile::ConfigHandle,
    before_commit: impl FnOnce(),
) {
    if singleton_held().unwrap_or(false) {
        return;
    }
    let snapshot = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = config.lock().expect("config mutex poisoned");
        cfg.clone()
    };
    let stamp = prior_generated_at().unwrap_or_else(|| crate::usage::epoch_secs_to_iso(0));
    let built_after = std::time::SystemTime::now();
    let Some(json) = status_feed_json(&snapshot, None, Some(&stamp)) else {
        return;
    };
    before_commit();
    publish_status_json_if_current(&snapshot, &json, built_after);
}

/// The daemon's last `generated_at`, read off the feed this publish replaces.
///
/// `None` when no feed is on disk, it does not parse, or its `generated_at` is
/// missing: no daemon has published a readable stamp here, and the caller
/// answers that with the epoch, which every staleness rule reads as "no
/// daemon".
fn prior_generated_at() -> Option<String> {
    let Ok(dir) = clauth_dir() else { return None };
    let body = std::fs::read(dir.join(STATUS_FILE)).ok()?;
    serde_json::from_slice::<serde_json::Value>(&body)
        .ok()?
        .get("generated_at")?
        .as_str()
        .map(str::to_string)
}

/// The `listen` slot the running daemon last published in `status.json`, for
/// `clauth devices pair`'s link. `None` when no feed is on disk, it does not
/// parse, or its writer serves no `--listen` API.
pub(crate) fn published_listen() -> Option<status_json::ListenSlot> {
    #[derive(serde::Deserialize)]
    struct Feed {
        #[serde(default)]
        listen: Option<status_json::ListenSlot>,
    }
    let body = std::fs::read(clauth_dir().ok()?.join(STATUS_FILE)).ok()?;
    serde_json::from_slice::<Feed>(&body).ok()?.listen
}

/// Rewrite `status.json` from `config`, unconditionally.
///
/// [`publish_status`]'s core, split out for the one caller that must NOT defer
/// to a running daemon: the daemon's own `POST /api/v1/switch`. There the daemon IS
/// the process that just switched, so there is no other owner to wait for, and
/// a client blocked on `GET /api/v1/status?wait=` is holding a connection open
/// precisely to be told the moment this file names the new account. Leaving it
/// to the next scheduler tick adds up to a second of nothing happening to every
/// switch made through the API.
///
/// `live` carries the scheduler's in-memory signals when the caller has them —
/// the daemon's own API switch does, and passing `None` there republished a feed
/// whose `fetch_status`, `next_refresh_at`, `stale` and `pending_switch` fell
/// back to the mtime derivation until the next tick overwrote them.
///
/// Best-effort and construction lock-placement rules are [`publish_status`]'s.
///
/// Stamps `generated_at` now: the daemon's own spelling, and itself the
/// freshness signal. Only the daemon's writers come through here; the
/// daemonless republish preserves the daemon's stamp instead
/// ([`publish_status_with`]).
pub(crate) fn write_status_feed(config: &AppConfig, live: Option<&LiveSignals>) {
    let Some(json) = status_feed_json(config, live, None) else {
        return;
    };
    write_status_json(&json);
}

/// A separate process can publish again while this body is built. Two guards,
/// taken with the write inside one brief State hold so daemonless publishers
/// serialize against each other: the persisted active marker must still match
/// the body's, and the on-disk feed must not be newer than `built_after`. The
/// publish branch is what the second guard licenses: a feed stamped strictly
/// before `built_after` finished its build before our build started, so its
/// inputs predate ours and ours is at least as fresh. Anything stamped at or
/// later skips — conservative even though a later stamp does not prove fresher
/// inputs (a slow writer can publish old inputs late), because a skip only
/// loses one best-effort refresh while a wrong overwrite is the
/// stale-whole-feed defect this guard exists to prevent (B→C→B and same-active
/// edits alike). The daemon's own writers take no state flock and bypass both
/// guards by design: they are the feed's owner while the singleton is held,
/// and this path defers to them at entry. Both guards only skip: publication
/// is best-effort after a switch that already succeeded. The recency
/// comparison leans on the filesystem stamp and `SystemTime` sharing one
/// realtime clock; a backward clock step, or a coarse fs stamp collapsing a
/// just-later write onto `built_after`'s tick, can cost one mis-ordered
/// publish until the next one lands.
fn publish_status_json_if_current(
    snapshot: &AppConfig,
    json: &[u8],
    built_after: std::time::SystemTime,
) {
    debug_assert!(!crate::lockorder::holds::<crate::lockorder::rank::Config>());
    let active = snapshot.state.active_profile.as_ref();
    let feed = clauth_dir().ok().map(|dir| dir.join(STATUS_FILE));
    if let Err(e) = crate::lock::with_state_lock(|_held| {
        if crate::profile::load_app_state()?.active_profile.as_ref() != active {
            logline!(
                "clauth: skipped the daemonless status.json republish: the active profile moved while this body was built"
            );
            return Ok(());
        }
        if let Some(path) = feed.as_ref()
            && let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified())
            && mtime >= built_after
        {
            logline!(
                "clauth: skipped the daemonless status.json republish: a newer publication landed while this body was built"
            );
            return Ok(());
        }
        write_status_json(json);
        Ok(())
    }) {
        logline!("clauth: failed to publish status.json after a switch: {e:#}");
    }
}

fn status_feed_json(
    config: &AppConfig,
    live: Option<&LiveSignals>,
    generated_at: Option<&str>,
) -> Option<Vec<u8>> {
    debug_assert!(!crate::lockorder::holds::<crate::lockorder::rank::State>());
    debug_assert!(!crate::lockorder::holds::<crate::lockorder::rank::Config>());
    // The daemonless republish carries no tps slot: its `age_secs` would
    // otherwise freeze at ~0 while the backdated stamp reads stale for hours.
    // The ruling makes the daemon the slot's only republisher, so the slot is
    // skipped (not even parsed) whenever the stamp is backdated.
    let mut body = status_json::build_status_impl(
        config,
        config.state.refresh_interval_ms,
        live,
        false,
        generated_at.is_none(),
    )
    .0;
    // The backdate this applies is the one deliberate exception to
    // `build_status`'s "the stamp never precedes a per-entry verdict instant"
    // ordering: a non-daemon publish carries a daemon's OLD stamp over entries
    // built now, which is the point — stamp freshness must keep meaning
    // "a daemon wrote this", never "some process did".
    if let Some(stamp) = generated_at {
        body.generated_at = stamp.to_string();
    }
    match serde_json::to_vec_pretty(&body) {
        Ok(json) => Some(json),
        Err(e) => {
            logline!("clauth: failed to serialize status.json after a switch: {e}");
            None
        }
    }
}

fn write_status_json(json: &[u8]) {
    let Ok(dir) = clauth_dir() else { return };
    if let Err(e) = mkdir_700(&dir) {
        logline!(
            "clauth: failed to prepare {} for status.json: {e}",
            dir.display()
        );
        return;
    }
    if let Err(e) = atomic_write_600(&dir.join(STATUS_FILE), json) {
        logline!("clauth: failed to publish status.json after a switch: {e}");
    }
}

/// True when the live credentials diverge from the active profile's stored chain
/// and it isn't a first-login adoption — the daemon cannot prompt, so it skips
/// the switch and leaves the resolution to the operator (TUI Divergence modal).
///
/// A logged-out shell (see [`crate::claude::live_credentials_are_shell`]) is
/// exempt: an empty login is not "unsaved credentials", and deferring on it
/// wedges every headless switch behind a TUI decision about nothing while
/// running sessions sit at "Login expired" (observed 2026-07-15). An
/// unreadable/torn live file still defers — it may be a CC write in progress.
/// The shell / first-login / clauth-symlink exemptions all live in
/// [`crate::claude::live_diverged_and_unsaved`]; a read that errors outright
/// maps to `false` (proceed) here.
fn active_diverged_unsaved(active: &crate::profile::ProfileName) -> bool {
    crate::claude::live_diverged_and_unsaved(active).unwrap_or(false)
}

/// The live scheduler stores a published feed's [`LiveSignals`] are built from.
///
/// Bundled so that anything publishing the feed takes the SAME snapshot the
/// daemon's own tick does. Without it the REST API had to pass `None` and fall
/// back to the mtime derivation, so `GET /api/v1/status?all=1` — which rebuilds
/// rather than serving the file — disagreed with the plain route on
/// `fetch_status`, `next_refresh_at`, `stale` and `pending_switch`, on the same
/// daemon, in the same second.
///
/// Every field an `Arc` clone, so handing one to the listener costs nothing and
/// shares the scheduler's state rather than copying it.
#[derive(Clone)]
pub(crate) struct LiveStores {
    pub(crate) usage_status: StatusStore,
    pub(crate) third_party_status: ThirdPartyStatusStore,
    pub(crate) third_party_streaks: ThirdPartyStreaks,
    pub(crate) next_refresh_per_profile: NextRefreshPerProfile,
    pub(crate) poll_streaks: PollStreaks,
    pub(crate) pending_switch: PendingSwitch,
    pub(crate) auto_start_queue: crate::usage::AutoStartQueueState,
    pub(crate) kick_blocks: KickBlocks,
    /// The OAuth usage store, for the canceled-exclusion the queue membership
    /// rule reads (the daemon's own context).
    pub(crate) usage_store: UsageStore,
    /// The gateway supervisor's published slot.
    pub(crate) gateway: gateway::GatewayHandle,
    /// The per-proxy supervisors' published slots.
    pub(crate) proxies: proxies::ProxySlots,
    /// The scheduler's token-spend store: the REST switch claims its refresh
    /// here, so a stopping daemon's drain waits that save out.
    pub(crate) activity: ActivityStore,
    /// The daemon's shutdown flag, which the same claim refuses once a stop
    /// signal has raised it.
    pub(crate) shutting_down: Arc<AtomicBool>,
    /// The usage-fetch lease, so every feed writer's snapshot carries whether
    /// THIS daemon runs the fetches: a stood-down daemon publishes
    /// `next_refresh_at: null` rather than the mirror's derived stamps.
    pub(crate) fetch_lease: Arc<FetchLease>,
    /// Where the `--listen` API serves, set once by its bind and read by every
    /// status write as the `listen` slot. Unset without `--listen`.
    pub(crate) listen: ListenHandle,
}

/// The `listen` slot's one writer is the API's bind, once per daemon, so a
/// set-once cell needs no lock rank.
pub(crate) type ListenHandle = Arc<OnceLock<status_json::ListenSlot>>;

#[cfg(test)]
impl Default for LiveStores {
    /// Empty stores, for a test that wants to seed one of them and prove a
    /// route reads it rather than deriving the same field from a file mtime.
    fn default() -> Self {
        Self {
            usage_status: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
            poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            pending_switch: Arc::new(RankedMutex::new(Default::default())),
            auto_start_queue: Arc::new(RankedMutex::new(Default::default())),
            kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
            usage_store: Arc::new(RankedMutex::new(HashMap::new())),
            gateway: gateway::new_handle(),
            proxies: proxies::new_slots(),
            activity: Arc::new(RankedMutex::new(HashMap::new())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            fetch_lease: Arc::new(FetchLease::new()),
            listen: ListenHandle::default(),
        }
    }
}

/// One consistent read of [`LiveStores`], owning its data so no lock is held
/// while the feed is built.
pub(crate) struct LiveSnapshot {
    status: HashMap<String, FetchStatus>,
    third_party_status: HashMap<String, FetchStatus>,
    third_party_streaks: HashMap<String, u32>,
    next_refresh: HashMap<LegKey, u64>,
    streaks: HashMap<String, u32>,
    pending_switch: Option<String>,
    queue_anchor: Option<i64>,
    queue_blocked: Vec<ProfileName>,
    usage: HashMap<String, UsageInfo>,
    gateway: Option<gateway::GatewaySlot>,
    proxies: Vec<proxies::ProxySlot>,
    /// Whether this daemon held the usage-fetch lease when the snapshot was
    /// taken — the flag every feed writer reads to null a stood-down
    /// daemon's countdowns.
    fetches_here: bool,
    listen: Option<status_json::ListenSlot>,
}

impl LiveStores {
    /// Snapshot every store, each lock released at the end of its own statement
    /// so none is ever held when CONFIG (which outranks all of them) is taken
    /// next, and none is held while [`build_status`] does its disk work.
    pub(crate) fn snapshot(&self) -> LiveSnapshot {
        let status = self
            .usage_status
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // The third-party leg writes its outcomes to a store of its own, so
        // without this snapshot every api-key/provider profile fell through to
        // the mtime derivation — and an `AuthExpired` session, which writes no
        // cache, published `fetch_status: null`, indistinguishable from a cold
        // start.
        let third_party_status = self
            .third_party_status
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let third_party_streaks = self
            .third_party_streaks
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let next_refresh = self
            .next_refresh_per_profile
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // Projected to the 429 axis on purpose: `stale` is contracted as a stuck
        // THROTTLE (`wiki/Daemon.md`), so a refresh-fail streak must not leak in.
        let streaks = self
            .poll_streaks
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.rate_limit)).collect())
            .unwrap_or_default();
        // The in-flight switch target (accepted, not yet applied), so a reader
        // shows in-flight truth instead of a timing heuristic. The set holds at
        // most one scheduler target in practice (`scan_auto_switch` skips while
        // one is pending); `min` keeps the snapshot deterministic anyway.
        let pending_switch = self.pending_switch.lock().ok().and_then(|q| {
            q.iter()
                .min_by(|a, b| a.target.cmp(&b.target))
                .map(|d| d.target.clone())
        });
        // `switch_grade_kick_lifts` keys ARE the blocked set: it and the
        // scheduler's own `kick_rejected_names` share one predicate, which is how
        // the TUI's queue chips read it too.
        let queue_anchor = crate::usage::queue_anchor_cached(&self.auto_start_queue);
        let queue_blocked: Vec<ProfileName> =
            crate::usage::switch_grade_kick_lifts(&self.kick_blocks)
                .keys()
                .map(|k| k.as_str().into())
                .collect();
        let usage = self
            .usage_store
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        LiveSnapshot {
            status,
            third_party_status,
            third_party_streaks,
            next_refresh,
            streaks,
            pending_switch,
            queue_anchor,
            queue_blocked,
            usage,
            gateway: gateway::published(&self.gateway),
            proxies: proxies::slots(&self.proxies),
            fetches_here: self.fetch_lease.fetches_here(),
            listen: self.listen.get().cloned(),
        }
    }
}

impl LiveSnapshot {
    pub(crate) fn signals(&self) -> LiveSignals<'_> {
        LiveSignals {
            fetches_here: self.fetches_here,
            status: &self.status,
            third_party_status: &self.third_party_status,
            third_party_streaks: &self.third_party_streaks,
            next_refresh: &self.next_refresh,
            streaks: &self.streaks,
            pending_switch: self.pending_switch.as_deref(),
            queue_anchor: self.queue_anchor,
            queue_blocked: &self.queue_blocked,
            usage: &self.usage,
            gateway: self.gateway.as_ref(),
            proxies: Some(&self.proxies),
            listen: self.listen.as_ref(),
        }
    }
}

/// The one freshness test the daemon's per-tick latches share: a `Fresh` fetch
/// status on a reading the feed does not call stale.
fn entry_fresh(entry: &status_json::ProfileEntry) -> bool {
    let fresh = status_json::fetch_status_str(crate::usage::FetchStatus::Fresh);
    !entry.stale && entry.fetch_status.as_deref() == Some(fresh)
}

/// [`entry_fresh`] as the 5h bell and the window push judge it: a proxy
/// member's freshness is its chain windows' (`alert_stale`).
fn entry_alert_fresh(entry: &status_json::ProfileEntry) -> bool {
    let fresh = status_json::fetch_status_str(crate::usage::FetchStatus::Fresh);
    !entry.alert_stale && entry.fetch_status.as_deref() == Some(fresh)
}

/// What a background hold keys on: the account's fixed pair, or one handed-off
/// per-launch tree by its sid. The two coexist on one account and never collide.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BackgroundKey {
    Account(ProfileName),
    Session(String),
}

impl BackgroundKey {
    /// The name a log line or a backoff/copy-host map labels this hold by.
    fn label(&self) -> &str {
        match self {
            BackgroundKey::Account(name) => name.as_str(),
            BackgroundKey::Session(sid) => sid,
        }
    }

    /// The runtime tree this key's hold adopts, if its source record still
    /// resolves: the configured account's fixed pair, or the handed-off row's
    /// start-profile per-session tree.
    fn tree(&self) -> Option<PathBuf> {
        match self {
            BackgroundKey::Account(name) => crate::runtime::background_runtime_tree(name).ok(),
            BackgroundKey::Session(sid) => {
                let row = crate::live_sessions::get(sid)?;
                crate::runtime::chain_session_tree(
                    &ProfileName::from(row.start_profile.as_str()),
                    sid,
                )
                .ok()
            }
        }
    }
}

/// The daemon's holds on the per-account background trees and the handed-off
/// per-launch fallback trees, and the worker that enters and leaves them off
/// the tick thread.
///
/// The tick only derives which holds need adopting or releasing — one job store
/// read plus one canonicalised lookup per candidate — and hands the work to the
/// worker, which may block on the rotation lock and the state flock for as long
/// as `acquire` and teardown legitimately take. The tick thread itself must
/// never wait: the run loop's watchdog aborts it at [`WATCHDOG_DEADLINE`]. When
/// the worker is gone (it panicked), the tick logs once and stops queueing —
/// never falling back to running an acquire or a teardown inline.
struct BackgroundHolds {
    /// One hold per key whose tree a Claude Code job names.
    held: HashMap<BackgroundKey, crate::runtime::ProfileRuntime>,
    /// Keys an adoption is in flight for, so a later tick does not queue a
    /// second one.
    pending: HashSet<BackgroundKey>,
    /// One entry per key whose last adoption failed: the retry gate and the
    /// error the current failure streak already announced.
    retry: HashMap<BackgroundKey, AdoptionFailure>,
    /// Keys refused on this host: their tree's link mode read as copy on an
    /// adoption attempt, so no background job can be adopted there. The value
    /// is the set of job ids naming the tree at the last arm, so a change to
    /// that set re-arms one re-check; it drives the one-time refusal log line
    /// and is pruned when the key's job set empties.
    copy_host: HashMap<BackgroundKey, HashSet<String>>,
    /// The next tick at which a refused key may be re-attempted ONCE, so a link
    /// mode that flipped to real is adopted. Re-armed when the set of job ids
    /// naming the tree changes.
    copy_host_next: HashMap<BackgroundKey, u64>,
    /// Test seam: how many adoption attempts this daemon queued, so a copy-host
    /// refusal is pinned as "no 5 s backoff retry" (exactly one attempt past
    /// the 5 s mark).
    #[cfg(test)]
    adoption_attempts: usize,
    work: Option<crossbeam_channel::Sender<BackgroundWork>>,
    done: crossbeam_channel::Receiver<BackgroundDone>,
    /// Set once the worker's channels report it gone: nothing more is queued,
    /// and no hold is ever torn down on the tick thread.
    worker_gone: bool,
}

enum BackgroundWork {
    Adopt {
        key: BackgroundKey,
        stale_env_keys: Vec<String>,
    },
    /// Handed over rather than dropped on the tick thread: teardown takes the
    /// state flock and joins a watchdog.
    Release(crate::runtime::ProfileRuntime),
}

enum BackgroundDone {
    Adopted(
        BackgroundKey,
        Box<anyhow::Result<crate::runtime::ProfileRuntime>>,
    ),
}

/// How long one hold waits after an adoption failure, and the error the current
/// streak logged. Doubling from [`ADOPT_BACKOFF_MIN_MS`], capped at
/// [`ADOPT_BACKOFF_MAX_MS`].
struct AdoptionFailure {
    attempts: u32,
    not_before_ms: u64,
    error: String,
}

const ADOPT_BACKOFF_MIN_MS: u64 = 5_000;
const ADOPT_BACKOFF_MAX_MS: u64 = 300_000;

/// How long a copy-host-refused key waits before its ONE re-attempt: long
/// enough to skip the 5 s backoff's first retry, short enough that a link-mode
/// flip (Developer Mode toggled) is adopted within a tick pass. A second
/// refusal does not re-arm it; only a job set that emptied and a new job does.
const COPY_HOST_RECHECK_MS: u64 = 10_000;

/// The wait after `attempts` consecutive adoption failures: the first wait is
/// the minimum, each further failure doubles it, capped, so a persistently
/// failing key stops taking the rotation lock and the state flock once per
/// tick.
fn adopt_backoff_ms(attempts: u32) -> u64 {
    ADOPT_BACKOFF_MIN_MS
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(6))
        .min(ADOPT_BACKOFF_MAX_MS)
}

/// Record one failed adoption for `key`. Returns the line to log when this is
/// the streak's first failure or a DIFFERENT error; an identical failure
/// repeating on the backoff schedule stays silent.
fn note_adoption_failure(
    retry: &mut HashMap<BackgroundKey, AdoptionFailure>,
    key: &BackgroundKey,
    error: &str,
    now_ms: u64,
) -> Option<String> {
    match retry.get_mut(key) {
        Some(failure) => {
            failure.attempts += 1;
            failure.not_before_ms = now_ms.saturating_add(adopt_backoff_ms(failure.attempts));
            let news = failure.error != error;
            if news {
                failure.error = error.to_string();
            }
            news.then(|| {
                format!(
                    "clauth daemon: adopting '{}' background tree failed again: {error}",
                    key.label()
                )
            })
        }
        None => {
            retry.insert(
                key.clone(),
                AdoptionFailure {
                    attempts: 1,
                    not_before_ms: now_ms.saturating_add(adopt_backoff_ms(1)),
                    error: error.to_string(),
                },
            );
            Some(format!(
                "clauth daemon: adopting '{}' background tree failed: {error}",
                key.label()
            ))
        }
    }
}

impl BackgroundHolds {
    fn new() -> Self {
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<BackgroundWork>();
        let (done_tx, done_rx) = crossbeam_channel::unbounded::<BackgroundDone>();
        #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
        std::thread::Builder::new()
            .name("clauth-bg-holds".to_string())
            .spawn(move || {
                while let Ok(item) = work_rx.recv() {
                    match item {
                        BackgroundWork::Adopt {
                            key,
                            stale_env_keys,
                        } => {
                            let outcome = match &key {
                                BackgroundKey::Account(name) => {
                                    crate::runtime::ProfileRuntime::acquire_background(
                                        name,
                                        crate::runtime::BackgroundHold::Daemon,
                                        &stale_env_keys,
                                        None,
                                    )
                                }
                                BackgroundKey::Session(sid) => {
                                    crate::runtime::ProfileRuntime::acquire_adopted(
                                        sid,
                                        &stale_env_keys,
                                    )
                                }
                            };
                            if done_tx
                                .send(BackgroundDone::Adopted(key, Box::new(outcome)))
                                .is_err()
                            {
                                return;
                            }
                        }
                        BackgroundWork::Release(runtime) => drop(runtime),
                    }
                }
            })
            .expect("failed to spawn the background-hold worker");
        Self {
            held: HashMap::new(),
            pending: HashSet::new(),
            retry: HashMap::new(),
            copy_host: HashMap::new(),
            copy_host_next: HashMap::new(),
            #[cfg(test)]
            adoption_attempts: 0,
            work: Some(work_tx),
            done: done_rx,
            worker_gone: false,
        }
    }

    /// Drain finished adoptions, then adopt every fixed pair a job names and
    /// every handed-off chain session a job names, and release every hold no
    /// job names any more. Never takes the rotation lock or the state flock.
    fn reconcile(&mut self, names: Vec<ProfileName>, stale_env_keys: Vec<String>) {
        let now = crate::usage::now_ms();
        self.drain_adoptions(now);
        let Some(work) = self.work.clone() else {
            return;
        };
        let jobs = crate::claude_jobs::job_trees();
        let released: Vec<BackgroundKey> = self
            .held
            .iter()
            .filter(|(key, _)| key.tree().is_none_or(|tree| !jobs.spares(&tree)))
            .map(|(key, _)| key.clone())
            .collect();
        for key in released {
            let Some(runtime) = self.held.remove(&key) else {
                continue;
            };
            if let Err(err) = work.send(BackgroundWork::Release(runtime)) {
                self.hold_back(err.into_inner(), key);
                self.worker_lost();
                return;
            }
            self.retry.remove(&key);
        }
        // An unreadable store names every tree, so it cannot say which holds to
        // adopt; leaving the holds as they are is the fail-closed direction
        // (nothing was released above either). A store caught mid-create names
        // no tree either — its record-less dir can name any tree the next
        // instant — so it adopts nothing new the same way; the release loop
        // above has already kept every hold.
        if matches!(jobs, crate::claude_jobs::JobTrees::Unreadable { .. })
            || jobs.global_mid_create_store().is_some()
        {
            return;
        }
        // A streak whose job has gone says nothing about the next one: clearing
        // it here keeps a stale backoff from delaying a hold that is being
        // adopted again. The copy-host gate and its one-shot re-check timer are
        // pruned the same way, so a job set that emptied and a NEW job naming
        // the tree re-arms one fresh re-check.
        self.retry
            .retain(|key, _| key.tree().is_some_and(|tree| jobs.spares(&tree)));
        self.copy_host
            .retain(|key, _| key.tree().is_some_and(|tree| jobs.spares(&tree)));
        self.copy_host_next
            .retain(|key, _| key.tree().is_some_and(|tree| jobs.spares(&tree)));
        // A refused key re-arms its ONE re-check whenever the set of job ids
        // naming its tree changes — a new job beside an older one, or a removed
        // one — not only when the set emptied. Storing the set at each arm
        // keeps it at most one re-check per distinct set, so a copy host never
        // turns into a backoff streak while a job keeps naming the tree.
        for (key, seen) in &mut self.copy_host {
            let Some(tree) = key.tree() else {
                continue;
            };
            let current = jobs.job_ids_naming(&tree);
            if current != *seen {
                *seen = current;
                self.copy_host_next
                    .insert(key.clone(), now.saturating_add(COPY_HOST_RECHECK_MS));
            }
        }
        // The fixed pair: adopt every configured account whose `runtime-bg` a
        // job names.
        for name in names {
            let key = BackgroundKey::Account(name.clone());
            if !self.queue_adoption(&work, key, &jobs, &stale_env_keys, now, None) {
                return;
            }
        }
        // The handed-off per-launch sessions: adopt every chain row a job names
        // whose marker no holder keeps. Lock-free (a registry read), so the
        // tick never takes the rotation lock or the state flock and never
        // probes the link mode.
        for row in crate::live_sessions::list() {
            if !row.follows_chain || row.isolated {
                continue;
            }
            let key = BackgroundKey::Session(row.session_id.clone());
            let Some(tree) = key.tree() else {
                continue;
            };
            let probe =
                ProfileName::from(row.current_member.as_deref().unwrap_or(&row.start_profile));
            if !self.queue_adoption(
                &work,
                key,
                &jobs,
                &stale_env_keys,
                now,
                Some((tree, probe, row.session_id.clone())),
            ) {
                return;
            }
        }
    }

    /// Queue one adoption if its gates clear: not already held or pending, past
    /// its copy-host re-check and its backoff, its tree still resolvable, its
    /// profile dir readable, and a job names its tree. For a session hold the
    /// optional `(tree, probe, sid)` also requires the marker be free. Returns
    /// false once the worker is lost.
    fn queue_adoption(
        &mut self,
        work: &crossbeam_channel::Sender<BackgroundWork>,
        key: BackgroundKey,
        jobs: &crate::claude_jobs::JobTrees,
        stale_env_keys: &[String],
        now: u64,
        session: Option<(PathBuf, ProfileName, String)>,
    ) -> bool {
        if self.held.contains_key(&key) || self.pending.contains(&key) {
            return true;
        }
        // A copy-host refusal skips the key until its one scheduled re-check (a
        // link-mode flip or a job-set change) or until its job set emptied and
        // a new job re-armed it.
        if self.copy_host.contains_key(&key) {
            let Some(next) = self.copy_host_next.get(&key) else {
                return true;
            };
            if now < *next {
                return true;
            }
            // The re-check is due: drop the gate so this tick re-queues the
            // adoption and the worker re-probes the mode under the lock.
            self.copy_host_next.remove(&key);
        }
        if self
            .retry
            .get(&key)
            .is_some_and(|failure| now < failure.not_before_ms)
        {
            return true;
        }
        let tree = match &session {
            Some((tree, _, _)) => Some(tree.clone()),
            None => key.tree(),
        };
        let Some(tree) = tree else {
            return true;
        };
        // A profile dir the job reader could not enumerate cannot be adopted:
        // the build reads its config and probes its link mode there, so
        // queueing it would start a failing backoff streak that re-takes the
        // locks every interval. Skipped, lock-free, and retried the moment the
        // dir reads again.
        if tree
            .parent()
            .is_some_and(|profile| jobs.profile_dir_unreadable(profile))
        {
            return true;
        }
        if !jobs.spares(&tree) {
            return true;
        }
        if let Some((_, probe, sid)) = &session
            && crate::runtime::session_row_is_live(probe, false, sid)
        {
            return true;
        }
        #[cfg(test)]
        {
            self.adoption_attempts += 1;
        }
        match work.send(BackgroundWork::Adopt {
            key: key.clone(),
            stale_env_keys: stale_env_keys.to_vec(),
        }) {
            Ok(()) => {
                self.pending.insert(key);
                true
            }
            Err(_) => {
                self.worker_lost();
                false
            }
        }
    }

    /// Take every finished adoption off the channel. A worker that exited (a
    /// panic) disconnects the channel; that is reported once and stops all
    /// further queueing rather than being retried forever.
    fn drain_adoptions(&mut self, now_ms: u64) {
        loop {
            match self.done.try_recv() {
                Ok(BackgroundDone::Adopted(key, outcome)) => {
                    self.pending.remove(&key);
                    match *outcome {
                        Ok(runtime) => {
                            self.retry.remove(&key);
                            self.held.insert(key, runtime);
                        }
                        Err(e) => {
                            let copy_host = e
                                .downcast_ref::<crate::runtime::CopyHostBackgroundRefusal>()
                                .is_some();
                            if copy_host {
                                // A designed refusal, not an adoption failure:
                                // log once, then let the tick arm ONE re-check
                                // (never the backoff's growing streak) when it
                                // sees the job-id set naming the tree, so a
                                // link-mode flip or a job-set change is adopted
                                // while a job keeps naming the tree. A second
                                // refusal neither re-logs nor re-arms it.
                                if !self.copy_host.contains_key(&key) {
                                    logline!(
                                        "clauth daemon: will not adopt '{}' background \
                                         tree on this host: it copies runtime trees instead \
                                         of linking them",
                                        key.label()
                                    );
                                    self.copy_host.insert(key.clone(), HashSet::new());
                                }
                            } else if let Some(line) = note_adoption_failure(
                                &mut self.retry,
                                &key,
                                &format!("{e:#}"),
                                now_ms,
                            ) {
                                logline!("{line}");
                            }
                        }
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.worker_lost();
                    return;
                }
            }
        }
    }

    /// The worker is gone: log once, drop the sender so no later tick can hand
    /// it work, and keep every hold where it is. Nothing here may tear a hold
    /// down — a teardown joins a watchdog and takes the state flock, which is
    /// exactly what the tick thread must not do.
    fn worker_lost(&mut self) {
        if self.worker_gone {
            return;
        }
        self.worker_gone = true;
        logline!(
            "clauth daemon: the background-hold worker stopped; no background tree will be \
             adopted or released until the daemon restarts"
        );
        self.pending.clear();
        self.work.take();
    }

    /// Put a runtime whose release could not be handed to the worker back under
    /// its hold, so the marker stays held and only the daemon's own drop tears
    /// it down.
    fn hold_back(&mut self, item: BackgroundWork, key: BackgroundKey) {
        if let BackgroundWork::Release(runtime) = item {
            self.held.insert(key, runtime);
        }
    }
}

impl Drop for BackgroundHolds {
    fn drop(&mut self) {
        // Close the work channel (the worker runs what is already queued, then
        // exits; a failed send back ends it early and the rest of the queue is
        // dropped unrun, a queued release tearing its runtime down there) and
        // tear each hold down on THIS thread, so a daemon dropping in-process
        // releases its markers before its own process state ends. A release
        // already handed to the worker finishes there. An adoption's runtime
        // already sent back is torn down here, when the `done` receiver drops
        // with this struct; one the worker finishes later fails its send and is
        // torn down on the worker. Nothing waits on the worker, which may sit on
        // a blocking rotation lock.
        drop(self.work.take());
        let held = std::mem::take(&mut self.held);
        drop(held);
    }
}

/// Owns the shared `Arc` stores (cloned into the scheduler) plus main-loop-only
/// state. Only the main thread touches `self`; the scheduler holds `Arc` clones
/// of the individual stores.
struct Daemon {
    config: ConfigHandle,
    usage_tokens: TokenList,
    usage_store: UsageStore,
    usage_status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    poll_streaks: PollStreaks,
    /// Per-profile kick-429 blocks. The daemon renders no pills, so this backs
    /// only the scheduler's own gate and its write-through cache files — but it
    /// lives on `self` rather than inside `spawn_scheduler` for the same reason
    /// `auto_start_queue` does: the status publisher snapshots its switch-grade
    /// subset on every write, because that subset is what the auto-start
    /// queue's membership rule excludes on and a feed deriving membership
    /// without it publishes a queue the election is not running.
    kick_blocks: KickBlocks,
    /// Interleaved auto-start queue (`usage::auto_start_queue`). On `self`, like
    /// `kick_blocks` above, because the status publisher snapshots the anchor on
    /// every write so `status.json`'s `next_open_at` matches the value the
    /// election gates on.
    auto_start_queue: crate::usage::AutoStartQueueState,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    third_party_streaks: ThirdPartyStreaks,
    third_party_broken: ThirdPartyBroken,
    skipped_unreadable: SkippedUnreadable,
    /// Scheduler-only stores held on `self` so `forget_memory` can reach the
    /// whole process's name-keyed state.
    weekly_reset_kicks: WeeklyResetKicks,
    /// The claude banked-reset side read's cadence memo, shared with the
    /// scheduler the same way [`Self::weekly_reset_kicks`] is.
    claude_resets_memo: ClaudeResetsMemoLock,
    /// The codex banked-reset side read's cadence memo, shared the same way.
    codex_resets_memo: CodexResetsMemoLock,
    suppressed_auth_expired: SuppressedAuthExpiredStore,
    shutting_down: Arc<AtomicBool>,
    /// The fetch lease the scheduler tick holds: the main-loop drains read it
    /// to tell whether this instance still fetches, so a queued switch or
    /// switch-off does not execute once the watchdog released the lease (the
    /// holder that took over decides).
    fetch_lease: Arc<FetchLease>,
    /// The scheduler's tick thread once `boot` started it: the stop gate
    /// ([`Self::spend_drain`]) carries it, so a stop waits for its exit save.
    scheduler: std::sync::OnceLock<TickThread>,
    /// Last-seen reload fingerprint — drives external-change reload. Set only
    /// by a successful reload (which re-read disk); a self-initiated write
    /// never moves it, so the next tick re-reads, rebuilds and logs.
    last_reload_fp: ReloadFingerprint,
    /// The text of the last logged config-reload failure — the dedup key that
    /// stops a permanently-broken config from re-logging every tick.
    last_reload_error: Option<String>,
    /// The identity snapshot the reload compare diffs against, shared with the
    /// REST route through `memory_stores`.
    snapshot: Arc<RankedMutex<crate::usage::SnapshotMap, crate::lockorder::rank::Snapshot>>,
    /// Epoch-ms of the last completed main-loop tick — the watchdog's liveness
    /// signal. `0` until the first tick completes.
    heartbeat: Arc<AtomicU64>,
    /// Backoff/dedup state for a persistently-failing switch: a target stuck on
    /// a transient failure retries on a widening schedule instead of 1/tick,
    /// and re-logs only when the target or reason changes.
    switch_backoff: Option<SwitchBackoff>,
    /// Count of ACTUAL failure-log emissions (post-dedup) — the observable proof a
    /// stuck switch isn't logging 1/tick. Read by tests.
    switch_failure_logs: u64,
    /// The accounts whose spent usage-credit cap has already been logged,
    /// until it clears or the account leaves the credit gate (its own
    /// endpoint).
    credit_capped: HashSet<String>,
    /// The credit refusals on record as the last [`Self::log_credit_refusals`]
    /// pass read them, so each edge is logged once and a tick re-reads only
    /// the records that moved.
    credit_refusals: crate::profile_cache::CreditRefusalWatch,
    /// The dead-login record stats the last [`crate::usage::sync_dead_logins`]
    /// pass read, so a tick re-reads only the records that moved.
    dead_login_stats: crate::usage::DeadLoginStats,
    /// The day-list notices last logged, empty while the lists are ordinary.
    /// Same dedup shape as `switch_backoff` above and for the same reason: the
    /// condition is re-derived every tick, so each message is its own key — it
    /// changes at the midnight rollover and on a config edit, and is
    /// byte-equal in between (`AppConfig::day_claim_notices_today`).
    day_claim_notices: Vec<String>,
    /// The unaddressable-name notices last logged, the same set-gate as
    /// `day_claim_notices` (`AppConfig::name_notices`).
    name_notices: Vec<crate::profile::NameNotice>,
    /// The kind of the last background-hold strip-list read that failed and
    /// was logged, `None` once one reads: the read runs every tick, so a
    /// failure logs once per kind rather than once per tick.
    strip_list_failure: Option<String>,
    /// The headless bell's latch over the published `profiles[]` figures.
    bells: crate::bell::BellLatch,
    /// The push sender's inbox, while the REST API serves; `None` otherwise.
    push: Option<crate::push::Queue>,
    /// The account and window latches each published body moves.
    push_latches: crate::push::StatusLatches,
    /// The last codex roster this daemon read successfully, in memory only;
    /// empty until the first successful read. On a failed read it stands in
    /// for the roster `queue_pushes` forgets against and `log_name_notices`
    /// judges, so a failed read forgets or re-raises no codex names on a guess.
    last_codex_roster: Vec<ProfileName>,
    /// Runs `bell_command`; a seam so tests record the ring instead.
    ring_bell: fn(&str, &str) -> anyhow::Result<()>,
    status_path: PathBuf,
    /// The slot the gateway supervisor publishes, read by every status write.
    gateway: gateway::GatewayHandle,
    /// The per-proxy slots every proxy supervisor publishes into, read by
    /// every status write as the `proxies` array.
    proxies: proxies::ProxySlots,
    /// Where the `--listen` API serves once bound, read by every status write.
    listen: ListenHandle,
    /// The daemon's holds on the per-account background trees.
    background: BackgroundHolds,
    /// Ticks since boot, for the bare-item leg's cadence
    /// ([`BARE_ITEM_SCAN_EVERY_TICKS`]) and the stale-copy sweep's
    /// ([`STALE_COPY_SWEEP_EVERY_TICKS`]).
    #[cfg(target_os = "macos")]
    bare_item_scan_ticks: u64,
    /// The listener start this daemon booted with, named by the cert-refused
    /// verdict's manual-restart advice (the re-exec check reads it).
    #[cfg(unix)]
    restart_advice: RestartAdvice,
}

impl Daemon {
    fn new(config: AppConfig, status_path: PathBuf) -> Self {
        // This process holds reading stores, so change sites record their
        // forgets here and the tick + REST handlers drain them.
        crate::usage::register_memory_stores();
        // Take the reload fingerprint BEFORE seeding the snapshot: a write
        // landing between the two then moves the fingerprint, so the first
        // reload re-reads config while the snapshot already holds the newer
        // identity — never a spurious forget off a stale snapshot.
        let last_reload_fp = reload_fingerprint();
        // Seed the identity snapshot from disk before the first compare, so a
        // freshly-started daemon forgets nothing its own startup already seeded.
        let snapshot = crate::usage::new_snapshot(
            &config
                .profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        );
        let usage_tokens: TokenList = Arc::new(RankedMutex::new(collect_tokens(&config)));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        // Seeded off the durable records once the config handle exists, below.
        let third_party_broken: ThirdPartyBroken = Arc::new(RankedMutex::new(HashMap::new()));
        let refresh_interval = Arc::new(AtomicU64::new(config.state.refresh_interval_ms));
        let daemon = Self {
            config: Arc::new(RankedMutex::new(config)),
            usage_tokens,
            usage_store: Arc::new(RankedMutex::new(HashMap::new())),
            usage_status: Arc::new(RankedMutex::new(HashMap::new())),
            refresh_interval,
            next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
            activity: Arc::new(RankedMutex::new(HashMap::new())),
            last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
            poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
            auto_start_queue: crate::usage::new_auto_start_queue_state(),
            pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
            pending_switch_off: Arc::new(RankedMutex::new(false)),
            refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
            third_party_tokens,
            third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_broken,
            skipped_unreadable: Arc::new(RankedMutex::new(HashSet::new())),
            weekly_reset_kicks: Arc::new(RankedMutex::new(HashSet::new())),
            claude_resets_memo: new_claude_resets_memo(),
            codex_resets_memo: new_codex_resets_memo(),
            suppressed_auth_expired: Arc::new(RankedMutex::new(HashMap::new())),
            // Raised by [`SpendDrain::drain`] when a stop signal arrives, and
            // by `serve` when its API fails to start after `boot`: from then
            // on the scheduler refuses every new token spend
            // (`spawn_refresher` and the rotation gate read it), while the
            // spend already in flight saves. The TUI's quit path is the flag's
            // other writer.
            shutting_down: Arc::new(AtomicBool::new(false)),
            fetch_lease: Arc::new(FetchLease::new()),
            scheduler: std::sync::OnceLock::new(),
            last_reload_fp,
            last_reload_error: None,
            snapshot,
            heartbeat: Arc::new(AtomicU64::new(0)),
            switch_backoff: None,
            switch_failure_logs: 0,
            day_claim_notices: Vec::new(),
            name_notices: Vec::new(),
            strip_list_failure: None,
            credit_capped: HashSet::new(),
            credit_refusals: crate::profile_cache::CreditRefusalWatch::default(),
            dead_login_stats: crate::usage::DeadLoginStats::new(),
            bells: crate::bell::BellLatch::default(),
            push: None,
            push_latches: crate::push::StatusLatches::default(),
            last_codex_roster: Vec::new(),
            ring_bell: crate::bell::run_command,
            status_path,
            gateway: gateway::new_handle(),
            proxies: proxies::new_slots(),
            listen: ListenHandle::default(),
            background: BackgroundHolds::new(),
            #[cfg(target_os = "macos")]
            bare_item_scan_ticks: 0,
            #[cfg(unix)]
            restart_advice: RestartAdvice::default(),
        };
        crate::usage::seed_dead_logins(&daemon.config, &daemon.third_party_broken);
        daemon
    }

    /// Re-establish the active profile's credential symlink, seed usage from the
    /// on-disk caches, and launch the scheduler. Mirrors the TUI's bootstrap.
    fn boot(&self) {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let active = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .state
            .active_profile
            .as_ref()
            .cloned();
        if let Some(active) = active {
            // A refused settle leaves the relink to its own guard, which
            // keeps an unsaved bare-item login in place; the leg retries it.
            // Nobody can answer a question here, so an item the settle
            // cannot place is refused the same way.
            #[cfg(target_os = "macos")]
            let _placed = {
                let writer = crate::claude::BareItemWriter::Relink;
                match crate::claude::settle_bare_keychain_item(writer) {
                    Ok(crate::claude::BareItemSettle::Settled(settled)) => {
                        if let Some(notice) = &settled.notice {
                            logline!("clauth daemon: {notice}");
                        }
                        Some(crate::claude::BareItemPlacement::enter(settled.placed))
                    }
                    Ok(crate::claude::BareItemSettle::NeedsChoice(unplaced)) => {
                        logline!("clauth daemon: {:#}", unplaced.refusal(writer));
                        None
                    }
                    Err(e) => {
                        logline!("clauth daemon: {e:#}");
                        None
                    }
                }
            };
            if let Err(e) = link_profile_credentials(&active) {
                logline!("clauth daemon: {e:#}; .credentials.json was not relinked to '{active}'");
            }
        }

        let interval = self.refresh_interval.load(Ordering::Relaxed);
        let (seed_names, third_party, third_party_work) = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = self.config.lock().expect("config mutex poisoned");
            let third_party_work = collect_third_party_entries(&cfg.profiles);
            // The daemon has no display reader of its provider store
            // (`status.json` reads provider figures off disk), so the boot seed
            // is the work list — a display-only name's cache buys nothing here
            // and must not reach the daemon's walk.
            let third_party: Vec<String> = third_party_work
                .iter()
                .map(|e| e.name.to_string())
                .collect();
            (
                collect_oauth_seed_names(&cfg, interval),
                third_party,
                third_party_work,
            )
        };
        bootstrap_fetch(
            &self.usage_store,
            &self.usage_status,
            &self.last_fetched,
            &seed_names,
        );
        bootstrap_third_party(
            &self.third_party_usage_store,
            &self.third_party_status,
            &self.last_fetched,
            &third_party,
            &third_party_work,
            interval,
        );
        self.spawn_scheduler();
    }

    /// The stop gate over this daemon's own scheduler store and shutdown flag:
    /// what a signal watcher carries, so the drain sees exactly the token
    /// spends claimed in this store and refuses the ones that have not started.
    fn spend_drain(&self) -> SpendDrain {
        let drain = SpendDrain::new(Arc::clone(&self.activity), Arc::clone(&self.shutting_down));
        match self.scheduler.get() {
            Some(scheduler) => drain.with_scheduler(scheduler.clone()),
            None => drain,
        }
    }

    /// Bundle scheduler `Arc`s and launch the background refresher (same call the
    /// TUI's `start_scheduler` makes).
    fn spawn_scheduler(&self) {
        let scheduler = spawn_refresher(
            Arc::clone(&self.config),
            Arc::clone(&self.usage_tokens),
            Arc::clone(&self.usage_store),
            Arc::clone(&self.usage_status),
            Arc::clone(&self.refresh_interval),
            Arc::clone(&self.next_refresh_per_profile),
            Arc::clone(&self.activity),
            Arc::clone(&self.last_fetched),
            Arc::clone(&self.poll_streaks),
            Arc::clone(&self.kick_blocks),
            Arc::clone(&self.auto_start_queue),
            Arc::clone(&self.weekly_reset_kicks),
            Arc::clone(&self.claude_resets_memo),
            Arc::clone(&self.codex_resets_memo),
            Arc::clone(&self.pending_switch),
            Arc::clone(&self.pending_switch_off),
            Arc::clone(&self.refetch_queue),
            Arc::clone(&self.third_party_tokens),
            Arc::clone(&self.third_party_usage_store),
            Arc::clone(&self.third_party_status),
            Arc::clone(&self.third_party_streaks),
            Arc::clone(&self.third_party_broken),
            Arc::clone(&self.skipped_unreadable),
            Arc::clone(&self.suppressed_auth_expired),
            Arc::clone(&self.shutting_down),
            // Single-fetcher lease (#27): the daemon competes for `usage-fetch.lock`
            // like any instance. It normally boots first (launchd) and wins the
            // lease, but if a TUI is already fetching, the daemon stands its
            // refresher down and hydrates instead — the main loop still writes
            // `status.json` every tick regardless of who fetches. The daemon's
            // own clone feeds the main-loop drains' stand-down check; the tick
            // thread's clone keeps the flock held while its ticks complete, and
            // its lease watchdog releases it when they stop.
            Arc::clone(&self.fetch_lease),
            // The daemon has no display reader of its provider store
            // (`status.json` reads provider figures off disk), so its stand-down
            // hydration seeds the work list, never the display superset.
            // Pinned by construction, not a test: the E2E `serve` stop child
            // drives this call site but never reads the provider store this
            // flag shapes.
            false,
            Arc::clone(&self.snapshot),
        );
        // `boot` runs once per daemon, so the cell is empty here.
        let _ = self.scheduler.set(scheduler);
    }

    /// Main loop. Writes an initial `status.json` immediately (so a reader that
    /// attaches before the first fetch has something to read), then each tick
    /// reloads external config changes, executes queued switches, and rewrites
    /// `status.json`. Runs until the process is killed.
    fn run(&mut self, stop_kit: &gateway::StopKit) {
        self.write_status();
        // Stamp the first heartbeat before the watchdog starts so it never trips
        // on the zero-heartbeat boot window, then spawn it.
        self.heartbeat
            .store(crate::usage::now_ms(), Ordering::Relaxed);
        self.spawn_watchdog();
        // daemon.log lives beside status.json; cap it on a ~5-min cadence (and at
        // boot, tick 0) so a pre-fix crash-loop log or a busy period can't grow it
        // unbounded. The check is a cheap stat that no-ops well
        // under the cap.
        let log_path = self.status_path.with_file_name(LOG_FILE);
        let mut ticks: u64 = 0;
        let mut reexec_watch = ReexecWatch::default();
        loop {
            if ticks.is_multiple_of(LOG_ROTATE_EVERY_TICKS) {
                let _ = log_rotate::rotate_log_if_large(
                    &log_path,
                    log_rotate::LOG_MAX_BYTES,
                    log_rotate::LOG_KEEP_BYTES,
                );
            }
            std::thread::sleep(TICK);
            self.tick();
            self.heartbeat
                .store(crate::usage::now_ms(), Ordering::Relaxed);
            // Reap the children the previous image left (the boot-captured
            // set) once their exits land, so none of them sits defunct for
            // the daemon's life; the set only ever holds pids this process
            // inherited across the exec.
            gateway::reap_inherited_tick();
            // The re-exec check sits between ticks, after the heartbeat store:
            // the scheduler thread may be mid-rotation at this instant, so the
            // stop path drains the spend in flight before the exec.
            #[cfg(unix)]
            reexec_if_due(
                &mut reexec_watch,
                stop_kit,
                &self.heartbeat,
                &self.restart_advice,
            );
            #[cfg(not(unix))]
            reexec_if_due(&mut reexec_watch, stop_kit, &self.heartbeat);
            // Copied bearers a stamp queued while a tick's budget was armed,
            // re-pointed off this loop so its Keychain legs never spend the
            // watchdog's window.
            crate::runtime::repoint_copied_bearers_detached();
            ticks = ticks.wrapping_add(1);
        }
    }

    /// Spawn the anti-wedge watchdog. It observes the main loop's tick
    /// heartbeat and `std::process::abort`s if a tick hasn't completed within
    /// [`WATCHDOG_DEADLINE`] — launchd's `KeepAlive{SuccessfulExit=false}` then
    /// restarts the daemon. This is the backstop for the deadline-free
    /// `StateLock`: a switch's `/usr/bin/security` subprocess or a wedged flock
    /// holder can otherwise freeze the single-threaded loop indefinitely, with
    /// nothing to restart a hung-but-alive process at 3am.
    fn spawn_watchdog(&self) {
        let heartbeat = Arc::clone(&self.heartbeat);
        let spawned = std::thread::Builder::new()
            .name("clauth-daemon-watchdog".into())
            .spawn(move || {
                let mut unaccounted_ms: u64 = 0;
                loop {
                    let slept_from = crate::usage::now_ms();
                    std::thread::sleep(WATCHDOG_POLL);
                    // A suspend/resume freezes this thread with the main loop
                    // while the wall clock keeps running; watchdog_step
                    // charges the overshot poll no running time, so the first
                    // post-resume check judges the pre-freeze heartbeat on
                    // time the loop actually had. The heartbeat is untouched:
                    // the main loop re-stamps on its next tick, and a loop
                    // that wedges before or after the resume still aborts
                    // within the deadline of running time.
                    unaccounted_ms = watchdog_step(
                        heartbeat.load(Ordering::Relaxed),
                        slept_from,
                        crate::usage::now_ms(),
                        unaccounted_ms,
                        WATCHDOG_POLL.as_millis() as u64,
                        WATCHDOG_DEADLINE.as_millis() as u64,
                        || {
                            logline!(
                                "clauth daemon: watchdog: no tick within {}s; aborting for a \
                                 clean launchd restart",
                                WATCHDOG_DEADLINE.as_secs()
                            );
                            std::process::abort();
                        },
                    );
                }
            });
        if let Err(e) = spawned {
            // No watchdog = a wedged loop hangs forever with launchd seeing a
            // live process. Say so loudly; the daemon still runs.
            logline!(
                "clauth daemon: failed to spawn the anti-wedge watchdog: {e}. \
                 A stalled tick will NOT auto-restart this process"
            );
        }
    }

    /// The live stores, bundled for anything that publishes the feed — the tick
    /// below and the REST API's rebuild alike, so the two cannot disagree.
    fn live_stores(&self) -> LiveStores {
        LiveStores {
            usage_status: Arc::clone(&self.usage_status),
            third_party_status: Arc::clone(&self.third_party_status),
            third_party_streaks: Arc::clone(&self.third_party_streaks),
            next_refresh_per_profile: Arc::clone(&self.next_refresh_per_profile),
            poll_streaks: Arc::clone(&self.poll_streaks),
            pending_switch: Arc::clone(&self.pending_switch),
            auto_start_queue: Arc::clone(&self.auto_start_queue),
            kick_blocks: Arc::clone(&self.kick_blocks),
            usage_store: Arc::clone(&self.usage_store),
            gateway: Arc::clone(&self.gateway),
            proxies: Arc::clone(&self.proxies),
            activity: Arc::clone(&self.activity),
            shutting_down: Arc::clone(&self.shutting_down),
            fetch_lease: Arc::clone(&self.fetch_lease),
            listen: Arc::clone(&self.listen),
        }
    }

    /// Bundle the reading stores `forget_memory` clears, cloning every `Arc`.
    fn memory_stores(&self) -> crate::usage::MemoryStores {
        crate::usage::MemoryStores {
            usage: Arc::clone(&self.usage_store),
            usage_status: Arc::clone(&self.usage_status),
            last_fetched: Arc::clone(&self.last_fetched),
            next_refresh: Arc::clone(&self.next_refresh_per_profile),
            poll_streaks: Arc::clone(&self.poll_streaks),
            kick_blocks: Arc::clone(&self.kick_blocks),
            weekly_reset_kicks: Arc::clone(&self.weekly_reset_kicks),
            claude_resets_memo: Arc::clone(&self.claude_resets_memo),
            codex_resets_memo: Arc::clone(&self.codex_resets_memo),
            auto_start_queue: Arc::clone(&self.auto_start_queue),
            third_party_usage: Arc::clone(&self.third_party_usage_store),
            third_party_status: Arc::clone(&self.third_party_status),
            third_party_streaks: Arc::clone(&self.third_party_streaks),
            third_party_broken: Arc::clone(&self.third_party_broken),
            suppressed: Arc::clone(&self.suppressed_auth_expired),
            interval_ms: Arc::clone(&self.refresh_interval),
            snapshot: Arc::clone(&self.snapshot),
        }
    }

    /// Drain the forgets a change site noted and apply each to memory, right
    /// after the state-flock and config guard drop.
    fn apply_forgets(&self) {
        crate::usage::apply_pending_forgets(&self.memory_stores());
    }

    /// Snapshot the live freshness/countdown stores — each snapshot's lock is
    /// fully released at the end of its own statement, so none is ever held
    /// when the `config` lock below is taken — then build and atomically
    /// write `status.json`.
    ///
    /// The config is snapshotted too: [`build_status`] stats and reads each
    /// profile's cache files and sweeps the session flocks, and holding CONFIG
    /// across that disk work every tick stalls every other config user (a switch,
    /// a TUI edit) behind it. The clone is a handful of small strings.
    ///
    /// Hands back the codex roster the body was built from, so the tick's name
    /// notices read it from this one load rather than a second.
    fn write_status(&mut self) -> Option<crate::codex_profiles::CodexState> {
        let interval = self.refresh_interval.load(Ordering::Relaxed);
        let snapshot = self.live_stores().snapshot();
        let live = snapshot.signals();
        let cfg_snap = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = self.config.lock().expect("config poisoned");
            cfg.clone()
        };
        // `false`: hide disabled accounts by default, matching `status_oneshot`.
        let (body, codex) =
            status_json::build_status_with_codex(&cfg_snap, interval, Some(&live), false);
        match serde_json::to_vec_pretty(&body) {
            Ok(json) => {
                if let Err(e) = atomic_write_600(&self.status_path, &json) {
                    logline!("clauth daemon: failed to write status.json: {e}");
                }
            }
            Err(e) => logline!("clauth daemon: failed to serialize status.json: {e}"),
        }
        self.log_credit_caps(&cfg_snap, &body.profiles);
        self.log_credit_refusals(&cfg_snap);
        self.ring_bells(
            &cfg_snap.profiles,
            &body.profiles,
            cfg_snap.state.bell_command.as_deref(),
        );
        self.queue_pushes(&cfg_snap, &body.profiles, codex.as_ref());
        codex
    }

    /// Say a spent usage-credit cap out loud, once per account per edge, on
    /// fresh readings only: the cap turns spent and it clears, and each edge
    /// gets exactly one line while the account bills its own usage credits
    /// (an own-endpoint account, or one the usage probe reads, is forgotten
    /// silently). Driven by the published body, so every daemon ticks it, API
    /// listener or not.
    /// The latch is in process, so a restart over a standing cap says it once
    /// more rather than swallowing the line after a crash (the bell's rule) —
    /// and an unchanged tick writes nothing.
    fn log_credit_caps(&mut self, config: &AppConfig, entries: &[status_json::ProfileEntry]) {
        // A deleted account's entry goes with it; one merely hidden from the
        // body (disabled) keeps its latch, so a re-enable does not re-log.
        self.credit_capped.retain(|name| {
            config
                .profiles
                .iter()
                .any(|p| p.name.as_str() == name.as_str())
        });
        // Read only on an edge: the flag lives in `~/.claude.json`.
        let families = || crate::claude_json::credit_billed_families().join(", ");
        for entry in entries {
            let name = entry.name.as_str();
            // A stale reading is no edge in either direction (the window
            // latches' rule).
            if !entry_fresh(entry) {
                continue;
            }
            // An account serving inference on its own endpoint is under no
            // credit gate. The push reads it as not capped, so the log forgets
            // its latch the same way — silently, since the clear copy would be
            // false here. A probe reading carries no credit figures, so it can
            // say neither edge either, and its account's latch goes the same way.
            if config.find(&entry.name).is_some_and(|p| {
                crate::claude::has_own_inference_endpoint(p) || crate::usage::usage_probe_admits(p)
            }) {
                self.credit_capped.remove(name);
                continue;
            }
            if !entry.credit_capped {
                if self.credit_capped.remove(name) {
                    logline!("{name}: credit cap clear, {} run again", families());
                }
            } else if self.credit_capped.insert(name.to_string()) {
                logline!(
                    "{name}: credit cap spent; Claude Code refuses {} on it until its monthly \
                     spend limit resets or is raised",
                    families()
                );
            }
        }
    }

    /// Say a credit refusal on record out loud, once per account per edge: it
    /// lands, and it clears. The record keeps an account off every automatic
    /// switch for a session that may need credits with no expiry, and nothing
    /// else a headless daemon writes names it. Read off disk, so a refusal the
    /// TUI placed while it fetched usage is said too. The latch is in process,
    /// as the cap's, so a restart over a standing refusal says it once more.
    fn log_credit_refusals(&mut self, config: &AppConfig) {
        let names: Vec<ProfileName> = config.profiles.iter().map(|p| p.name.clone()).collect();
        for (name, on_record) in self.credit_refusals.refresh(&names) {
            if on_record {
                logline!(
                    "{name}: credits refused; the global walk takes it after its headroom and home \
                     passes, and a session that may need credits after every other pass, until a \
                     request on it succeeds or a window reset leaves it headroom"
                );
            } else {
                logline!(
                    "{name}: credit refusal cleared; auto-switch takes it again for the global \
                     account and for any session"
                );
            }
        }
    }

    /// Hand the push sender what this body changed: an account entering a
    /// failed login or key state (`broken`: a refresh rejected as revoked or
    /// invalid; `AuthExpired`: a usage credential that will not heal, an api
    /// key or console session the provider rejected or a setup token the server
    /// refused to the usage probe; an `expired` access token only waits for its
    /// rotation), and a chain
    /// member's window reaching the line the chain switches it at, on a fresh
    /// reading, the bell's own rule: the 5h window at its `threshold`, the long
    /// window (7d, else the provider-marked monthly quota) at its weekly line;
    /// a codex chain member at the lines the codex chain
    /// switches it at (`fallback::snapshot_codex_chain`: the default 5h
    /// threshold, the codex weekly line), and an account's usage-credit cap
    /// filling on a fresh reading. Nothing is queued while the API is off.
    fn queue_pushes(
        &mut self,
        config: &AppConfig,
        entries: &[status_json::ProfileEntry],
        codex: Option<&crate::codex_profiles::CodexState>,
    ) {
        let Some(queue) = &self.push else { return };
        let auth_expired = status_json::fetch_status_str(crate::usage::FetchStatus::AuthExpired);
        let chain_weekly = config.state.weekly_switch_threshold_pct();
        let codex_member = |entry: &status_json::ProfileEntry| {
            codex.filter(|codex| {
                entry.harness == crate::harness::Harness::Codex.as_str()
                    && codex.fallback_chain().contains(&entry.name)
            })
        };
        let util = |entry: &status_json::ProfileEntry, label: &str| {
            entry
                .windows
                .iter()
                .find(|w| w.label == label)
                .map(|w| w.utilization_pct)
        };
        // The long window's util, read at the presence label the entry already
        // carries from `build_status` (the same single cache read its published
        // rows derive from), never at whichever row survived the liveness
        // filter — a lapsed 7d window reads the 7d label and thus no published
        // row, instead of falling to the monthly row. Codex windows are always
        // 5h/7d, so a codex member keeps its 7d row.
        let long_util = |entry: &status_json::ProfileEntry| {
            if codex_member(entry).is_some() {
                return util(entry, crate::usage::LABEL_7D);
            }
            entry.long_window_label.and_then(|label| util(entry, label))
        };
        let own_endpoint = |entry: &status_json::ProfileEntry| {
            config
                .find(&entry.name)
                .is_some_and(crate::claude::has_own_inference_endpoint)
        };
        let readings = entries.iter().map(|entry| crate::push::Reading {
            name: entry.name.as_str(),
            failing: entry.auth_status == "broken"
                || entry.fetch_status.as_deref() == Some(auth_expired),
            credit_capped: entry.credit_capped && !own_endpoint(entry),
            five_hour: crate::push::WindowReading {
                line: entry
                    .fallback
                    .as_ref()
                    .map(|fallback| fallback.threshold)
                    .or_else(|| codex_member(entry).map(|_| crate::fallback::DEFAULT_THRESHOLD)),
                util: util(entry, crate::usage::LABEL_5H),
            },
            seven_day: crate::push::WindowReading {
                line: entry
                    .fallback
                    .as_ref()
                    .and(config.find(&entry.name))
                    .map(|profile| crate::fallback::member_weekly_line(profile, chain_weekly))
                    .or_else(|| {
                        codex_member(entry)
                            .map(crate::codex_profiles::CodexState::weekly_switch_threshold_pct)
                    }),
                // The long window — 7d, else the published 30d monthly row —
                // judges at the member's weekly line (the ruling's "window push
                // follows that line too").
                util: long_util(entry),
            },
            fresh: entry_alert_fresh(entry),
        });
        // Forget against the ROSTER, not the published body: a disabled account
        // is hidden from the body but is still an account, so its latches must
        // survive the hide. The codex half is the last roster this daemon read
        // successfully (in memory), so a failed read forgets no codex names on
        // a guess.
        if let Some(codex) = codex {
            self.last_codex_roster.clear();
            self.last_codex_roster.extend_from_slice(codex.profiles());
        }
        let roster = config
            .profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .chain(self.last_codex_roster.iter().map(|name| name.as_str()));
        self.push_latches.forget_absent(roster);
        for trigger in self.push_latches.observe(readings) {
            queue.send(trigger);
        }
    }

    /// The headless half of the 5h bell, judged on the figures this tick just
    /// published so the bell and the feed never disagree about a figure; a
    /// proxy member's freshness is its chain windows' (`alert_stale`). With no
    /// `bell_command` a present account keeps its latch as it is, so setting
    /// one mid-crossing does not re-announce a bell already rung; a deleted
    /// account is forgotten either way.
    fn ring_bells(
        &mut self,
        roster: &[crate::profile::Profile],
        entries: &[status_json::ProfileEntry],
        command: Option<&str>,
    ) {
        // Forget against the ROSTER, not the published body: a disabled account
        // is hidden from the body but is still an account, so its standing bell
        // must not re-arm when it comes back into view. Only a deleted account
        // forgets. Codex has no `bell_threshold`, so the claude roster is the
        // whole bell population.
        let present: HashSet<&str> = roster.iter().map(|profile| profile.name.as_str()).collect();
        self.bells.forget_absent(&present);
        let Some(command) = command else { return };
        for entry in entries {
            let fresh = entry_alert_fresh(entry);
            let util = entry
                .windows
                .iter()
                .find(|w| w.label == crate::usage::LABEL_5H)
                .map(|w| w.utilization_pct);
            let Some(message) =
                self.bells
                    .observe(entry.name.as_str(), entry.bell_threshold, util, fresh)
            else {
                continue;
            };
            if let Err(e) = (self.ring_bell)(command, &message) {
                logline!("clauth daemon: alert for '{}' failed: {e:#}", entry.name);
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_watchdog.rs"]
mod watchdog_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_mod.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_reexec.rs"]
mod reexec_tests;

#[cfg(all(test, unix))]
#[path = "../../tests/inline/daemon_reexec_steps.rs"]
mod reexec_steps_tests;

#[cfg(all(test, unix))]
#[path = "../../tests/inline/daemon_handover.rs"]
mod handover_tests;
