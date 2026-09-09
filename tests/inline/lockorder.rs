//! The rank check is the mechanism every other lock-discipline claim leans on.
//!
//! `the_ttl_clock_is_reachable_under_the_config_guard` is written on the premise
//! that a misorder panics there. But the whole enforcement is one
//! `debug_assert!` inside `RankGuard::enter`, and nothing asserted that it
//! actually fires: a refactor that dropped the check, or loosened `>` to `>=`,
//! would compile, keep every lock test green, and silently downgrade the order
//! to prose again — the exact failure the module was built to end.
//!
//! These pin the mechanism itself, both directions. `debug_assertions`-gated
//! because the check compiles out in release, where the guard is a no-op by
//! design (a `--release` test run must not fail for the absence of a panic).

use super::*;

/// The inversion that deadlocks: `Rotation` (100) is held across the OAuth HTTP
/// round trip and is outermost by design, while `Config` (400) is held across
/// the TUI's account-swap actions. Taking rotation *under* config is the order
/// inversion — it must panic in dev/test rather than deadlock in production.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "lock-order violation")]
fn acquiring_an_outer_rank_while_holding_an_inner_one_panics() {
    let _config = RankGuard::enter::<rank::Config>();
    let _rotation = RankGuard::enter::<rank::Rotation>();
}

/// The comparison is strictly `>`, not `>=`. Re-entering the same rank means
/// taking the same non-reentrant mutex twice, which self-deadlocks — so the
/// equal case must panic too. Loosening the assert to `>=` would still pass
/// every ascending-order test in the tree; only this one catches it.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "lock-order violation")]
fn re_entering_the_same_rank_panics() {
    let _config = RankGuard::enter::<rank::Config>();
    let _config_again = RankGuard::enter::<rank::Config>();
}

/// The documented nesting (`config` → state flock → `activity`, with rotation
/// outermost) is legal, and dropping the guards pops them: a sequential
/// acquire-then-drop constrains nothing, so the lowest rank is acquirable again
/// afterwards. Without this the two `should_panic` tests above would also pass
/// against a guard that panicked unconditionally.
#[test]
fn ranks_entered_outermost_first_are_legal_and_release_on_drop() {
    {
        let _rotation = RankGuard::enter::<rank::Rotation>();
        let _config = RankGuard::enter::<rank::Config>();
        let _state = RankGuard::enter::<rank::State>();
        let _activity = RankGuard::enter::<rank::Activity>();
    }
    // Every guard above is dropped, so the stack is empty and the outermost
    // rank is legal again — proving Drop pops rather than leaking the rank.
    let _rotation_again = RankGuard::enter::<rank::Rotation>();
    let _config_again = RankGuard::enter::<rank::Config>();
}

/// The two `cfg(test)` scaffolding ranks close a LATENT deadlock: no test grabs
/// both the home sandbox and the tier pin today, so an inverted acquisition
/// would sail through the whole gate and only deadlock a future test that does.
/// `HomeTest` is ranked outer to `TierTest`, so pinning the tier under a held
/// home sandbox is the legal ascending order. Nothing else exercises these two
/// ranks, so a value swap that reopened the deadlock would pass every other
/// test — only this asserts the order (it panics here if the ranks invert).
#[test]
fn a_tier_pin_is_legal_under_a_home_sandbox() {
    let _home = RankGuard::enter::<rank::HomeTest>();
    let _tier = RankGuard::enter::<rank::TierTest>();
}
