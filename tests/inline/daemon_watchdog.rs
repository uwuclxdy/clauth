//! TECH-3 — anti-wedge watchdog decision (`watchdog_check`).
//!
//! Pure tests of the abort decision: `on_stall` is injected as a flag so the
//! stall path is asserted without a real `std::process::abort()`. The production
//! wiring (heartbeat store each tick + a thread that calls `abort`) is a thin
//! loop around this predicate.

use super::watchdog_check;

/// A tick that last completed longer ago than the deadline trips `on_stall`.
#[test]
fn watchdog_aborts_when_tick_stalls_past_deadline() {
    let mut stalled = false;
    // last tick at t=1000ms; now t=1000+61_000ms; deadline 60_000ms → 61s > 60s.
    watchdog_check(1_000, 1_000 + 61_000, 60_000, || stalled = true);
    assert!(
        stalled,
        "a tick stalled past the deadline must trip the abort path"
    );
}

/// A tick within the deadline does not trip.
#[test]
fn watchdog_does_not_abort_on_a_fresh_tick() {
    let mut stalled = false;
    watchdog_check(1_000, 1_000 + 30_000, 60_000, || stalled = true);
    assert!(!stalled, "a tick within the deadline must not abort");
}

/// Exactly at the deadline is not yet stalled (strict `>`), so it holds one more
/// poll — the boundary must not abort a daemon that just barely ticked.
#[test]
fn watchdog_boundary_is_not_stalled() {
    let mut stalled = false;
    watchdog_check(1_000, 1_000 + 60_000, 60_000, || stalled = true);
    assert!(!stalled, "exactly at the deadline is not past it");
}

/// A zero heartbeat means no tick has completed yet (boot in progress); the
/// watchdog must never abort during that window.
#[test]
fn watchdog_ignores_zero_heartbeat_during_boot() {
    let mut stalled = false;
    watchdog_check(0, 999_999_999, 60_000, || stalled = true);
    assert!(!stalled, "a zero heartbeat (no tick yet) must not abort");
}
