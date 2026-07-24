//! Small, pure daemon state types surfaced additively in `status.json`, plus the
//! switch-backoff schedule. Extracted from `mod.rs` to keep the run-loop module
//! under the size gate; all are re-exported from `super` so callers (tick.rs,
//! socket.rs, status_json.rs, the inline tests) reference them unchanged.

use crate::fallback_config::MoveDir;

/// A fallback-config edit queued by the control socket for the main loop to
/// apply on its next tick — mirrors how `switch` enqueues into `pending_switch`
/// so *all* config mutation stays on the daemon's main thread. Names are already
/// resolved to canonical form by the socket before enqueueing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfigOp {
    FallbackAdd(String),
    FallbackRemove(String),
    FallbackMove(String, MoveDir),
    SetThreshold(String, f64),
    /// Set/clear the exclusive last-resort mark on a chain member's profile.
    SetLastResort(String, bool),
    /// Set (`Some`) or clear (`None`) a member's per-account weekly-line
    /// override (`weekly at`).
    SetMemberWeekly(String, Option<f64>),
    /// Flip a member's usage gate: `(name, scoped, on)` — scoped=false is the
    /// `weekly gate` (`check_weekly`), scoped=true the `scoped gate`.
    SetUsageGate(String, bool, bool),
    SetWrapOff(bool),
    /// Set the chain-wide weekly (7d) exhaustion line (percent).
    SetWeeklyThreshold(f64),
    /// Rename a profile: `(canonical old, validated new)`. Renames the profile
    /// directory + every state reference; re-links the credential mirror if active.
    Rename(String, String),
}

/// The most recent reason a `drain_pending_switch` skipped, re-queued, or failed a
/// switch (TECH-6). Surfaced additively in `status.json` (`last_error`) so a tap
/// that can't land immediately — target mid-fetch, active diverged, auth revoked —
/// is observable ('no silent failures') instead of vanishing after the `{ok:true}`
/// ack. Main-thread-only state: written by the drain, read by `write_status`.
#[derive(Debug, Clone)]
pub(crate) struct LastError {
    /// Epoch-ms the reason was recorded.
    pub(crate) at_ms: u64,
    pub(crate) message: String,
}

/// The last executed switch (TECH-8) — the hero event, surfaced additively in
/// `status.json` (`last_switch`) so ccsbar can notify/banner it instead of it
/// living only in `daemon.log`. `to` is `None` for a wrap-off. Main-thread-only.
#[derive(Debug, Clone)]
pub(crate) struct LastSwitch {
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
    pub(crate) at_ms: u64,
    /// What drove it: `"user"` (socket tap), `"scheduler"` (auto), `"wrap_off"`.
    pub(crate) trigger: &'static str,
}

/// Retry/backoff state for a persistently-failing switch (TECH-8, finding #38).
/// Without it, a switch that can't land (keychain denial, diverged active, busy
/// target) re-queued and logged ~1/s forever, ballooning `daemon.log` and burying
/// the signal. This dedups the log (emit only when the reason changes) and spaces
/// retries with exponential backoff once a target has failed repeatedly.
#[derive(Debug, Clone)]
pub(crate) struct SwitchBackoff {
    pub(crate) target: String,
    /// Consecutive failed attempts for this target.
    pub(crate) attempts: u32,
    /// Epoch-ms before which no further attempt is made (backoff window).
    pub(crate) not_before: u64,
    /// Last failure reason for this target — the dedup key for logging.
    pub(crate) reason: String,
}

/// Base backoff after a switch has failed enough times to warrant spacing.
const SWITCH_BACKOFF_BASE_MS: u64 = 2_000;
/// Ceiling so a permanently-stuck target still gets an occasional retry.
const SWITCH_BACKOFF_MAX_MS: u64 = 60_000;

/// Backoff (ms) before the next attempt after `attempts` consecutive failures.
/// The first two failures retry immediately (`0`) so the common brief-fetch case
/// still lands the instant the target goes idle (preserves the TECH-6 re-queue
/// behavior); only a persistently-failing switch backs off, exponentially, capped.
/// Pure so the schedule is unit-testable.
pub(crate) fn switch_backoff_ms(attempts: u32) -> u64 {
    match attempts {
        0..=2 => 0,
        n => SWITCH_BACKOFF_BASE_MS
            .saturating_mul(1_u64 << (n - 3).min(20))
            .min(SWITCH_BACKOFF_MAX_MS),
    }
}
