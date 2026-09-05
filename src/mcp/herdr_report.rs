//! Best-effort herdr pane metadata reports carrying the delegate state of the
//! pane this MCP server runs in (`herdr pane report-metadata`).
//!
//! herdr injects `HERDR_PANE_ID` and `HERDR_BIN_PATH` into every pane process,
//! so the server (a Claude Code child) inherits its pane id. While a
//! `delegate` is in flight the pane carries a `clauth_delegate` metadata token
//! reading `working`; when the last in-flight delegate ends it reads `idle`.
//! One process-local counter tracks sync and background delegates together, so
//! a background job finalizing mid-run can never clear the reading while a
//! sync delegate still runs.
//!
//! The token is the form picked for this API. `pane report-agent` is dropped
//! on every pane herdr's own Claude Code integration has anchored (all
//! operator panes; re-measured 2026-08-25 on herdr 0.8.2: exit 0, no state
//! change), and metadata cannot move the agent-state icon there either — that
//! icon is herdr's own lifecycle authority's, and a metadata report writes
//! presentation only (tokens, title, display agent, state-label text;
//! measured 2026-08-25 on 0.8.2: a report carrying `--state-label` and
//! `--display-agent` applied at exit 0 and `agent_status` did not move).
//! `--state-label` relabels states herdr itself computes instead of reporting
//! one, so a background delegate — the case the counter exists for — could
//! never light it: the pane's own agent is idle while the delegate runs. The
//! token key `clauth_delegate` is distinct from the `clauth` key the
//! herdr-plugin's profile tag owns; both spell `--source clauth`, and herdr
//! 0.8.2 merges tokens per key and expires them per key (both measured
//! 2026-08-25 on an anchored pane: a probe token applied beside the standing
//! `clauth` tag, and a 20 s TTL cleared it on its own clock while the tag's
//! watcher kept re-reporting beside it).
//!
//! The icon itself stays herdr's own: on a pane its integration has anchored
//! the icon moves with the pane's agent's own activity, and a metadata report
//! never moves it, so this module never tries. What the token does is carry
//! the delegate state for the cases the icon cannot: a background delegate
//! (the pane's own agent sits idle while the delegate spends), an unanchored
//! pane, and a dead server (the TTL clears the stale `working`). herdr renders
//! a token only where a sidebar row template names it — the same rule the
//! profile tag lives under — and the row `clauth herdr install` writes
//! (`herdr.rs`) names `$clauth` alone, the profile tag, unless the
//! `delegate_row_text` knob is on, which appends `$clauth_delegate` to it.
//! With the knob off the state reads on the pane JSON (`pane get` / `pane
//! list`), not on the pane row. Measured 2026-08-25 on 0.8.2: an applied
//! token rendered nowhere on the agent row, and rendered `working` beside the
//! account tag when a test template named the token.
//!
//! Every report is best-effort: a failed or hanging herdr spawn never fails a
//! delegate. It does cost time, and the serve runtime is
//! `new_current_thread()`, so the cost is the whole server's. Only the
//! `working` half is charged to that thread — it runs at commit-to-launch,
//! before the delegate reaches `spawn_blocking` — while `idle` rides the run's
//! own blocking task, where a hung herdr delays nothing but that task's own
//! end. The refresher below runs on its own detached thread, never the serve
//! thread. Failures are silent except for one `logline`
//! each (the MCP stdio channel carries only the JSON-RPC frame on stdout, and
//! `logline` routes off it — to the log file on an interactive pane, stderr
//! otherwise).
//!
//! Gating: [`PaneReporter::resolve`] returns `None` unless the `delegate_dot`
//! knob is on AND BOTH `HERDR_PANE_ID` is present AND the herdr binary
//! resolves — `HERDR_BIN_PATH` when set (a path must exist), else `herdr`
//! found on `PATH` (the same resolution `crate::herdr::herdr_bin` names). The
//! knob reads once at server start off the on-demand config, defaulting on
//! when profiles.toml is missing or unreadable. Resolution happens once, in
//! the serve path (`ClauthServer::with_herdr_pane`); a server built without
//! it is a silent no-op.
//!
//! TTL and refresh: every report carries `--ttl-ms` [`STATE_TTL_MS`], so the
//! token self-clears with no exit path running — a server that dies
//! mid-delegate leaves a stale `working` for at most that long, where the old
//! icon path relied on herdr reclaiming the state at agent exit. A delegate
//! can run far longer than any single TTL, so while anything is in flight a
//! refresher thread re-reports `working` every [`REFRESH_INTERVAL`] (four
//! chances inside one TTL). It wakes on its own clock, mints each seq under
//! the same [`Gate`] lock as the transitions — so the final `idle` always
//! outranks the last refresh, and herdr's per-source high-water drops a stale
//! `working` that lands after it — and holds the reporter through a `Weak`,
//! so the thread ends once the last reporter clone drops. The `idle` report
//! replaces `working` at once and then self-clears at the TTL.
//!
//! Ceiling (survives): two clauth servers sharing one pane both spell
//! `--source clauth`, the token key is shared, and an epoch-ms seq is
//! comparable across processes, so one session's `idle` can outrank the
//! other's live `working` and clear the reading under it. Only two
//! independent Claude Code sessions in one pane reach that; a delegate's own
//! `claude` cannot, since the depth guard refuses it a second `delegate`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::logline::logline;

/// How long one report may block its caller on a hung herdr before the child
/// is killed and the report dropped. Bounds the worst-case delay a stuck herdr
/// adds to a delegate (two reports per run) and the worst case of the
/// refresher thread.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// The TTL every state report carries: the token self-clears this long after
/// its report unless a fresher report replaces it first. Bounds the stale
/// `working` a dead server leaves behind, and clears the `idle` reading once
/// it has served its glimpse.
const STATE_TTL_MS: u64 = 60_000;

/// How often the refresher re-reports `working` while anything is in flight.
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// The ratio is the liveness margin: four chances inside one TTL, so three
/// refreshes can fail before the reading lapses. Compile-time, so an edit
/// cannot stretch the interval past that margin under a green gate.
const _: () = assert!(STATE_TTL_MS >= REFRESH_INTERVAL.as_millis() as u64 * 4);

/// The metadata token key carrying the state. Distinct from `clauth`, the key
/// the herdr-plugin's profile tag owns (`report-profile.sh`); herdr merges
/// tokens per key within one source, so a shared `--source clauth` lets the
/// state read beside the profile tag instead of replacing it.
const TOKEN_KEY: &str = "clauth_delegate";

/// Process-local delegate tracking for one herdr pane. Cheap to clone: every
/// handle shares the same counters.
#[derive(Clone)]
pub(crate) struct PaneReporter {
    shared: Arc<Shared>,
}

struct Shared {
    bin: PathBuf,
    pane_id: String,
    gate: Mutex<Gate>,
}

/// The in-flight count and the seq clock under ONE lock: herdr keeps a
/// per-source high-water seq and drops anything not newer, so two reports whose
/// seqs order against their transitions leave the pane holding the loser's
/// state. Deciding and minting apart cannot give that ordering, whatever each
/// half is individually atomic over. Refreshes mint here too, so the final
/// `idle` always outranks the last `working` re-report.
#[derive(Default)]
struct Gate {
    /// In-flight delegates (sync and background). Reports fire on the 0→1
    /// (`working`) and →0 (`idle`) transitions only.
    in_flight: u64,
    last_seq: u64,
}

impl Gate {
    /// Epoch-ms, forced past the last mint. herdr's high-water survives this
    /// process, so a restart's first report has to beat what the previous
    /// process left behind; a same-millisecond pair still has to separate.
    fn mint(&mut self) -> u64 {
        let seq = crate::usage::now_ms().max(self.last_seq.saturating_add(1));
        self.last_seq = seq;
        seq
    }
}

impl PaneReporter {
    /// `Some` only when the `delegate_dot` knob is on, the pane env is
    /// present, and the herdr binary resolves. A knob-off server is the same
    /// silent no-op as a missing pane id.
    pub(crate) fn resolve(delegate_dot: bool) -> Option<Self> {
        if !delegate_dot {
            return None;
        }
        let pane_id = std::env::var("HERDR_PANE_ID").ok()?;
        if pane_id.trim().is_empty() {
            return None;
        }
        let bin = crate::herdr::resolved_bin()?;
        let shared = Arc::new(Shared {
            bin,
            pane_id,
            gate: Mutex::new(Gate::default()),
        });
        spawn_refresher(&shared, REFRESH_INTERVAL);
        Some(Self { shared })
    }

    /// One in-flight delegate began: report `working` on the 0→1 transition.
    pub(crate) fn begin(&self) {
        if let Some(seq) = self.enter() {
            report(&self.shared, "working", seq);
        }
    }

    /// One in-flight delegate ended: report `idle` on the →0 transition.
    fn end(&self) {
        if let Some(seq) = self.leave() {
            report(&self.shared, "idle", seq);
        }
    }

    /// Count one delegate in, minting its seq under the same lock that decided
    /// to report. `None` when something else is already in flight.
    fn enter(&self) -> Option<u64> {
        let mut gate = self.gate();
        gate.in_flight = gate.in_flight.saturating_add(1);
        (gate.in_flight == 1).then(|| gate.mint())
    }

    /// Count one delegate out, minting its seq under the same lock. `None`
    /// when work remains in flight.
    fn leave(&self) -> Option<u64> {
        let mut gate = self.gate();
        debug_assert!(
            gate.in_flight > 0,
            "herdr pane reporter: end with no matching begin"
        );
        // Checked, not wrapping: an unpaired end would otherwise leave a count
        // no later `idle` can ever reach.
        let rest = gate.in_flight.checked_sub(1)?;
        gate.in_flight = rest;
        (rest == 0).then(|| gate.mint())
    }

    /// The lock is held for the count and the mint, never across the herdr
    /// spawn or its wait. A poisoned gate keeps reporting: only the debug
    /// assert above can panic under it, and the counts it protects are cosmetic.
    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.shared
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The transitions without their reports. Every report costs a subprocess
    /// spawn, which is orders of magnitude wider than the window a seq minted
    /// off the decision would invert in, so a fixture that reports cannot
    /// observe the ordering these two promise.
    ///
    /// `unix` tracks the gate on the only suite that calls them: its shim herdr
    /// is POSIX shell, so the whole test module compiles out on the Windows leg
    /// and an ungated helper reds that leg alone under `-D warnings`.
    #[cfg(all(test, unix))]
    pub(super) fn enter_for_test(&self) -> Option<u64> {
        self.enter()
    }

    #[cfg(all(test, unix))]
    pub(super) fn leave_for_test(&self) -> Option<u64> {
        self.leave()
    }

    /// The refresher with a test-sized interval, on top of the real one (which
    /// never fires inside a test's run time). `unix` as above.
    #[cfg(all(test, unix))]
    pub(super) fn spawn_refresher_for_test(&self, interval: Duration) {
        spawn_refresher(&self.shared, interval);
    }
}

/// Detached re-reporter behind the `working` reading: sleeps `interval`, then
/// re-reports `working` when anything is in flight. Its own thread with a
/// `Weak` hold on the reporter: it can never stall the serve thread or the
/// run's task, its worst case is its own [`REPORT_TIMEOUT`], and it ends once
/// the last reporter clone drops.
fn spawn_refresher(shared: &Arc<Shared>, interval: Duration) {
    let weak = Arc::downgrade(shared);
    let spawned = std::thread::Builder::new()
        .name("clauth-herdr-refresh".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                let seq = {
                    let mut gate = shared.gate.lock().unwrap_or_else(PoisonError::into_inner);
                    (gate.in_flight > 0).then(|| gate.mint())
                };
                if let Some(seq) = seq {
                    report(&shared, "working", seq);
                }
            }
        });
    if spawned.is_err() {
        logline!(
            "clauth: herdr pane refresh thread failed to spawn (working reads will not refresh; the TTL still bounds staleness)"
        );
    }
}

/// One report to the pane: spawn `herdr pane report-metadata` and wait up to
/// [`REPORT_TIMEOUT`]. Every failure is swallowed — the pane token is
/// cosmetic, so a broken herdr must cost the delegate nothing.
fn report(shared: &Shared, state: &str, seq: u64) {
    // Pane id FIRST: herdr's hand-rolled parser reads it as args[0] and
    // answers `unknown option` (exit 2) to anything else in that slot.
    let token = format!("{TOKEN_KEY}={state}");
    let mut cmd = Command::new(&shared.bin);
    cmd.args(["pane", "report-metadata"])
        .arg(&shared.pane_id)
        .args(["--source", "clauth", "--token", &token])
        .arg("--ttl-ms")
        .arg(STATE_TTL_MS.to_string())
        .arg("--seq")
        .arg(seq.to_string())
        // Never leak herdr's output into the MCP channel or the console.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        logline!(
            "clauth: herdr pane report-metadata spawn failed (pane state {state} not reported)"
        );
        return;
    };
    let deadline = Instant::now() + REPORT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    logline!(
                        "clauth: herdr pane report-metadata exited {status} (pane state {state} not reported)"
                    );
                }
                return;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                logline!(
                    "clauth: herdr pane report-metadata timed out (killed; pane state {state} not reported)"
                );
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            // `Child`'s drop detaches without waiting, so a failed
            // `waitpid` (ECHILD, or EINTR on some libc paths) would leave
            // a zombie for the life of the server. Reap here as every
            // other arm does.
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// RAII in-flight tracker half: the drop reports `idle` once nothing else is in
/// flight, on every exit path, panic included.
pub(crate) struct InFlightGuard {
    reporter: PaneReporter,
}

impl InFlightGuard {
    /// Track only — every delegate's `begin` runs at commit-to-launch, and the
    /// guard is created first thing in the run's own task so no early return
    /// can skip the decrement and so the panel follows the RUN rather than the
    /// call that started it.
    pub(crate) fn end_only(reporter: PaneReporter) -> Self {
        Self { reporter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.reporter.end();
    }
}
