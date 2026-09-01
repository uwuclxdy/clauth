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

pub(crate) mod log_rotate;
mod probe;
mod status_json;
mod tick;
mod types;

use probe::{Claim, DaemonLock, StandbySlot, claim_singleton};
/// The single-fetcher lease + the header dot's daemon presence/health probe
/// (dual-scheduler dedup, #27).
pub(crate) use probe::{DaemonHealth, FetchLease, daemon_health, singleton_held};
#[cfg(test)]
pub(crate) use probe::{daemon_lock_path, hold_daemon_lock};
/// Small daemon state types + the backoff schedule, re-exported so callers keep
/// referencing them as `super::…` after the extraction.
pub(crate) use types::{SwitchBackoff, switch_backoff_ms};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::claude::link_profile_credentials;
use crate::lockorder::RankedMutex;
use crate::logline::logline;
use crate::out::outln;
use crate::profile::{
    AppConfig, ConfigHandle, ReloadFingerprint, atomic_write_600, clauth_dir, load_config,
    mkdir_700, reload_fingerprint,
};
use crate::usage::{
    ActivityStore, FetchStatus, KickBlocks, LastFetchedAt, NextRefreshPerProfile, PendingSwitch,
    PendingSwitchOff, PollStreaks, RefetchQueue, StatusStore, SuppressedGenericStore,
    ThirdPartyList, ThirdPartyStatusStore, ThirdPartyUsageStore, TokenList, UsageStore,
    bootstrap_fetch, bootstrap_third_party, collect_oauth_seed_names, collect_third_party_entries,
    collect_tokens, spawn_refresher,
};
use status_json::LiveSignals;
// `clauth list` (src/list.rs) renders a human table over the same body, so the
// two surfaces read one code path and cannot drift.
pub(crate) use status_json::{ProfileEntry, build_profile_entries, build_status};

/// Main-loop cadence. The scheduler ticks on its own 1s timer; this loop only
/// executes queued switches/config edits and rewrites `status.json`, so 1s is plenty.
const TICK: Duration = Duration::from_secs(1);
/// How often (in `TICK`s) the run loop checks `daemon.log` for size-capping
/// ~5 min at the 1s tick — rare enough to be free, frequent enough to
/// bound a busy log.
const LOG_ROTATE_EVERY_TICKS: u64 = 300;
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
/// held for life by whichever instance (daemon or a TUI) is the current usage
/// fetcher. See [`FetchLease`](probe::FetchLease).
const FETCH_LOCK_FILE: &str = "usage-fetch.lock";

/// Anti-wedge watchdog: abort if no tick completes within this window.
/// `TICK` is 1s, so ~30 missed ticks. The blocking `StateLock` has no deadline
/// and a switch runs a `/usr/bin/security` subprocess inside it, so a stuck
/// keychain or a wedged flock holder can freeze the single-threaded run loop;
/// `std::process::abort()` then lets launchd's `KeepAlive{SuccessfulExit=false}`
/// restart the daemon (boot()'s relink + atomic writes make restart safe).
///
/// Tightened 60s→30s for the single-fetcher lease (#27): a wedged-alive daemon
/// keeps holding `usage-fetch.lock`, so no other instance can fetch until it
/// dies — 30s frees the lease about as fast as the retired TUI freshness re-arm
/// did. TENSION: a legit switch's keychain shell-outs can block inside the
/// `StateLock`, leaving only ~10s of slack. Twice now the count of those
/// shell-outs grew under this deadline (a read-modify-write made one mirror two
/// calls; a first-login adopt makes one switch two mirrors), so the bound is no
/// longer per-call: `lock::SUBPROCESS_BUDGET` caps everything ONE state-lock
/// hold spends in `security` at 20s in aggregate. If it ever false-aborts,
/// shrink THAT (the real fix), do NOT loosen this deadline — the lease's
/// wedged-daemon recovery depends on it. Known residual:
/// the budget is per HOLD, and `tick.rs` drains `pending_switch` then
/// `pending_switch_off` under two separate holds, so one tick doing both can
/// still spend 2 × 20s here.
///
/// SCOPE: `heartbeat` is stamped by the MAIN loop only, so this covers a wedged
/// main loop. A wedged SCHEDULER thread (which is what actually holds the lease)
/// keeps the main loop ticking and the feed fresh, so it trips nothing and the
/// lease is never freed — pre-existing (the retired probe keyed on the same
/// main-loop freshness). Do not read this deadline as
/// covering the fetch path itself.
const WATCHDOG_DEADLINE: Duration = Duration::from_secs(30);
/// How often the watchdog re-checks the tick heartbeat.
const WATCHDOG_POLL: Duration = Duration::from_secs(10);

/// One watchdog evaluation: if the main loop last completed a tick more than
/// `deadline_ms` ago, invoke `on_stall`. Production passes `std::process::abort`;
/// tests inject a flag/panic. A zero `last_tick_ms` (no tick yet — boot in
/// progress) never trips. Pure so the abort decision is unit-testable.
fn watchdog_check(last_tick_ms: u64, now_ms: u64, deadline_ms: u64, on_stall: impl FnOnce()) {
    if last_tick_ms != 0 && now_ms.saturating_sub(last_tick_ms) > deadline_ms {
        on_stall();
    }
}

/// Tighten an existing `~/.clauth` tree on boot, before `load_config` runs its
/// own walk. `mkdir_700` only sets the mode on dirs it CREATES; a tree from an
/// older build or created under a permissive umask can be 0o755
/// (world-traversable → world-readable `daemon.log`, enumerable account names).
/// Delegates to [`crate::profile::enforce_clauth_perms`] for the whole tree
/// (dirs → 0o700, files → 0o600, symlinks skipped), which also covers the
/// launchd-created `daemon.log`: launchd opens it (`StandardErrorPath`) at the
/// umask (~0o644) before `exec`, and the already-open fd keeps appending to the
/// now-0o600 inode. Best-effort — a chmod failure never stops the daemon.
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

/// `clauth daemon` — build the shared stores, run the scheduler headless, and
/// loop executing auto-switches + rewriting `status.json` until killed.
pub(crate) fn serve(mode: StartMode) -> Result<()> {
    // First thing, before any output (including the standing-by line below):
    // daemon stderr IS daemon.log, and undated lines cost real forensics time
    // (2026-07-09 — see `logline`).
    crate::logline::enable_timestamps();
    crate::platform::init();

    let dir = clauth_dir()?;
    // Create ~/.clauth at 0o700 (was create_dir_all → umask 0o755). Above the
    // singleton claim because the lock files live in the dir; it no-ops for
    // every instance after the first.
    mkdir_700(&dir).context("failed to create ~/.clauth")?;

    // Single-instance guard, claimed BEFORE any shared-tree work below: a
    // redundant instance must not GC the live daemon's runtime forest or walk
    // its modes. The default exits the moment the lock is lost; only `--standby`
    // parks a lone waiter so a supervisor's instance can take over from a
    // manually run one (a clean exit is never restarted under launchd
    // `KeepAlive{SuccessfulExit=false}`); `--replace` terminates the holder and
    // takes over (#57). A dead holder's advisory flock auto-releases, so neither
    // a standby nor a replace is ever orphaned.
    let claim = match mode {
        StartMode::Replace => probe::claim_by_replacing(&dir)?,
        _ => claim_singleton(&dir, mode == StartMode::Standby)?,
    };
    let _lock = match claim {
        Claim::Active(lock) => lock,
        Claim::Standby(slot) => stand_by(&dir, slot)?,
        Claim::Redundant => {
            logline!("clauth daemon: {}; exiting", redundant_reason(mode));
            return Ok(());
        }
    };

    log_rotate::warn_if_log_cap_defeated();
    // Tighten an existing looser tree (older builds / CLI umask left it 0o755)
    // before `load_config` runs its own walk. Idempotent, so the standby path
    // running it twice costs one stat walk.
    migrate_clauth_perms_700(&dir);
    crate::runtime::gc_stale_runtimes();

    let config = load_config()?;
    warn_if_spend_is_uncapped(&config);
    let mut daemon = Daemon::new(config, dir.join(STATUS_FILE));
    daemon.boot();
    logline!(
        "clauth daemon: running (status → {})",
        daemon.status_path.display()
    );
    daemon.run();
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
/// (`next_target` skips it as a candidate), so it can't be the uncapped
/// spender this names. Auth-broken and canceled members stay named even though
/// that same walk skips them too: both clear on their own (a re-login, a
/// re-subscribe), so going quiet about a member one re-auth away from billing
/// errs the wrong way. `spend_is_uncapped` excludes them for the opposite
/// reason: there they would count as a SINK catching the spend, where a
/// hopeful read invents a safety net that isn't there.
fn uncapped_spenders(config: &crate::profile::AppConfig) -> Vec<&str> {
    config
        .state
        .fallback_chain
        .iter()
        .filter_map(|name| config.find(name))
        .filter(|p| !p.is_disabled())
        .filter(|p| crate::fallback::spend_is_uncapped(config, p.max_auto_spend.unwrap_or(0.0)))
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
    // The presence DECISION goes through `singleton_held`, not the header dot's
    // `daemon_health`: the dot maps an unusable lock to `Absent` so it can hide
    // rather than assert a daemon that may not be there, and a `--status ||
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
/// exits. No scheduler; freshness and next-refresh are derived from cache
/// mtimes. `include_disabled` mirrors `build_status`'s flag of the same name
/// (hidden by default; `dispatch`'s `--all`/`--disabled` flips it).
pub(crate) fn status_oneshot(include_disabled: bool) -> Result<()> {
    let config = load_config()?;
    let interval = config.state.refresh_interval_ms;
    let body = build_status(&config, interval, None, include_disabled);
    outln!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
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
    shutting_down: Arc<AtomicBool>,
    /// Last-seen reload fingerprint (`profiles.toml` mtime + per-account
    /// config.toml count/newest-mtime) — drives external-change reload. Bumped to
    /// the post-write value after every self-initiated switch so the daemon never
    /// reloads its own write.
    last_reload_fp: ReloadFingerprint,
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
    status_path: PathBuf,
}

impl Daemon {
    fn new(config: AppConfig, status_path: PathBuf) -> Self {
        let usage_tokens: TokenList = Arc::new(RankedMutex::new(collect_tokens(&config)));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        let refresh_interval = Arc::new(AtomicU64::new(config.state.refresh_interval_ms));
        Self {
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
            // Never set by the daemon: process exit IS its shutdown (a
            // supervisor restarts crashes; the singleton flock releases on
            // exit). The flag exists for `spawn_refresher`'s contract — its
            // real writer is the TUI's quit path.
            shutting_down: Arc::new(AtomicBool::new(false)),
            last_reload_fp: reload_fingerprint(),
            heartbeat: Arc::new(AtomicU64::new(0)),
            switch_backoff: None,
            switch_failure_logs: 0,
            status_path,
        }
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
            let _ = link_profile_credentials(&active);
        }

        let (seed_names, third_party) = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = self.config.lock().expect("config mutex poisoned");
            (
                collect_oauth_seed_names(&cfg),
                collect_third_party_entries(&cfg.profiles),
            )
        };
        let interval = self.refresh_interval.load(Ordering::Relaxed);
        bootstrap_fetch(
            &self.usage_store,
            &self.usage_status,
            &self.last_fetched,
            &seed_names,
            interval,
        );
        bootstrap_third_party(
            &self.third_party_usage_store,
            &self.third_party_status,
            &self.last_fetched,
            &third_party,
            interval,
        );
        self.spawn_scheduler();
    }

    /// Bundle scheduler `Arc`s and launch the background refresher (same call the
    /// TUI's `start_scheduler` makes). The suppressed-generic set is daemon-local.
    fn spawn_scheduler(&self) {
        let suppressed: SuppressedGenericStore = Arc::new(RankedMutex::new(HashMap::new()));
        spawn_refresher(
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
            Arc::clone(&self.pending_switch),
            Arc::clone(&self.pending_switch_off),
            Arc::clone(&self.refetch_queue),
            Arc::clone(&self.third_party_tokens),
            Arc::clone(&self.third_party_usage_store),
            Arc::clone(&self.third_party_status),
            suppressed,
            Arc::clone(&self.shutting_down),
            // Single-fetcher lease (#27): the daemon competes for `usage-fetch.lock`
            // like any instance. It normally boots first (launchd) and wins the
            // lease for life, but if a TUI is already fetching, the daemon stands
            // its refresher down and hydrates instead — the main loop still writes
            // `status.json` every tick regardless of who fetches. A fresh lease per
            // scheduler; the tick thread's clone keeps the flock held for the
            // process lifetime.
            Arc::new(FetchLease::new()),
        );
    }

    /// Main loop. Writes an initial `status.json` immediately (so a reader that
    /// attaches before the first fetch has something to read), then each tick
    /// reloads external config changes, executes queued switches, and rewrites
    /// `status.json`. Runs until the process is killed.
    fn run(&mut self) {
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
        let log_path = self.status_path.with_file_name("daemon.log");
        let mut ticks: u64 = 0;
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
                loop {
                    std::thread::sleep(WATCHDOG_POLL);
                    watchdog_check(
                        heartbeat.load(Ordering::Relaxed),
                        crate::usage::now_ms(),
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

    /// Snapshot the live freshness/countdown stores — each snapshot's lock is
    /// fully released at the end of its own statement, so none is ever held
    /// when the `config` lock below is taken — then build and atomically
    /// write `status.json`.
    ///
    /// The config is snapshotted too: [`build_status`] stats and reads each
    /// profile's cache files and sweeps the session flocks, and holding CONFIG
    /// across that disk work every tick stalls every other config user (a switch,
    /// a TUI edit) behind it. The clone is a handful of small strings.
    fn write_status(&self) {
        let interval = self.refresh_interval.load(Ordering::Relaxed);
        let status_snap: HashMap<String, FetchStatus> = self
            .usage_status
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // The third-party leg writes its outcomes to a store of its own, so
        // without this snapshot every api-key/provider profile fell through to
        // the mtime derivation — and an `AuthExpired` session, which writes no
        // cache, published `fetch_status: null`, indistinguishable from a cold
        // start.
        let tp_status_snap: HashMap<String, FetchStatus> = self
            .third_party_status
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let next_snap: HashMap<String, u64> = self
            .next_refresh_per_profile
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // Snapshot the 429 streaks so build_status can publish `stale` (a
        // deep-slot stuck RateLimited). Lower rank than config, like the stores
        // above — snapshot + release before the config lock. Projected to the 429
        // axis on purpose: `stale` is contracted as a stuck THROTTLE
        // (`wiki/Daemon.md`), so a refresh-fail streak must not leak into it.
        let streaks_snap: HashMap<String, u32> = self
            .poll_streaks
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.rate_limit)).collect())
            .unwrap_or_default();
        // The in-flight switch target (accepted, not yet applied), so the UI
        // shows in-flight truth instead of a timing heuristic. Snapshot + release
        // before the config lock, like the freshness stores above. The set holds
        // at most one scheduler target in practice (`scan_auto_switch` skips
        // while one is pending); `min` keeps the snapshot deterministic anyway.
        let pending_snap: Option<String> = self
            .pending_switch
            .lock()
            .ok()
            .and_then(|q| q.iter().min().cloned());
        // The auto-start queue's in-memory anchor and the blocks its membership
        // rule excludes on — both leaf rank, snapshot + release before the config
        // lock like every store above. `switch_grade_kick_lifts` keys ARE the
        // blocked set: it and the scheduler's own `kick_rejected_names` share one
        // predicate, which is how the TUI's queue chips read it too.
        let queue_anchor = crate::usage::queue_anchor_cached(&self.auto_start_queue);
        let queue_blocked: Vec<crate::profile::ProfileName> =
            crate::usage::switch_grade_kick_lifts(&self.kick_blocks)
                .keys()
                .map(|k| k.as_str().into())
                .collect();
        let live = LiveSignals {
            status: &status_snap,
            third_party_status: &tp_status_snap,
            next_refresh: &next_snap,
            streaks: &streaks_snap,
            pending_switch: pending_snap.as_deref(),
            queue_anchor,
            queue_blocked: &queue_blocked,
        };
        let cfg_snap = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = self.config.lock().expect("config poisoned");
            cfg.clone()
        };
        // `false`: hide disabled accounts by default, matching `status_oneshot`.
        let body = build_status(&cfg_snap, interval, Some(&live), false);
        match serde_json::to_vec_pretty(&body) {
            Ok(json) => {
                if let Err(e) = atomic_write_600(&self.status_path, &json) {
                    logline!("clauth daemon: failed to write status.json: {e}");
                }
            }
            Err(e) => logline!("clauth daemon: failed to serialize status.json: {e}"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_watchdog.rs"]
mod watchdog_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_mod.rs"]
mod tests;
