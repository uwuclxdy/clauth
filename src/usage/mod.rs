mod auto_start_queue;
mod burn;
mod fetch;
mod scheduler;

pub(crate) use burn::{
    BURN_GAP_CUT_MS, BURN_LOOKBACK_MS, BURN_MIN_SAMPLES, compute_burn_rates_from_history,
    project_utilization,
};
#[allow(unused_imports)]
pub(crate) use fetch::{
    ANTHROPIC_ORIGIN, ExtraPeriod, ExtraUsage, LABEL_5H, LABEL_7D, LoginProfile, PlanInfo,
    PlanTier, ScopedWindow, SpendInfo, UsageInfo, UsageWindow, WindowDollars, await_request_slot,
    cli_user_agent, epoch_secs_to_iso, expire_profile_ttl, fetch_account_uuid, five_hour_live,
    http_agent, humanize_duration, ideal_pace_pct, iso_to_epoch_secs, now_epoch_secs, now_ms,
    parse_retry_after, parse_retry_after_at, probe_login_profile, seed_login_anchor,
    seven_day_live, spent_resume_in_secs, window_avg_pace_per_day, windows_maxed,
};
pub(crate) use scheduler::{
    ActivityStore, FetchStatus, KickBlock, KickBlocks, LastFetchedAt, NextRefreshPerProfile,
    OpResult, OpResultReceiver, OpResultSender, PendingSwitch, PendingSwitchOff, PollStreaks,
    ProfileActivity, RefetchQueue, StartupReceiver, StartupSender, StartupSignal, StatusStore,
    StreakCounts, SuppressedGenericStore, ThirdPartyList, ThirdPartyStatusStore,
    ThirdPartyUsageStore, TokenList, UsageStore, any_busy, bootstrap_fetch, bootstrap_third_party,
    clear_activity, collect_oauth_seed_names, collect_third_party_entries, collect_tokens, is_idle,
    is_stuck_rate_limited, is_stuck_streak, kick_block_switch_grade, mark_activity,
    profile_credential_fingerprint, spawn_refresher, switch_gate_in_flight,
    switch_grade_kick_blocked_from_cache, switch_grade_kick_lifts, third_party_credentialed,
};
// The queue's history-pair classifier stays module-private — reached by
// `usage::auto_start_queue`'s own tests through `super::` — while the gap arithmetic is
// re-exported: the scheduler sizes the gap itself, and `status_json` derives
// `next_open_at` through [`auto_start_queue::next_queue_open_secs`].
pub(crate) use auto_start_queue::{
    AutoStartQueueState, Candidate, QueueSlot, auto_start_queue_members, elect_queue_member,
    history_anchor, new_state as new_auto_start_queue_state, next_queue_open_secs,
    note_queue_kick_failed, note_queue_open, queue_anchor, queue_anchor_cached, queue_due,
    queue_failures, queue_gap_secs, queue_slot, seed_queue_anchor,
};
// The active-cap boundary is only referenced by tests (production code reaches it
// through `is_stuck_rate_limited`); gate the re-export behind `cfg(test)` so it
// isn't a dead symbol in the shipped binary, while keeping the `stale`/distrust
// tests robust against a change to the constant's value.
#[cfg(test)]
pub(crate) use scheduler::ACTIVE_CAP_MAX_STREAK;
pub(crate) use scheduler::MAX_RETRY_AFTER_MS;
// Test-only: reset the per-host request-spacing slots so a real-bytes wire test
// driving a builder through `await_request_slot` doesn't sleep out the window,
// and read one back so a leg's reservation is assertable without that sleep.
#[cfg(test)]
pub(crate) use fetch::{reserved_request_slot, reset_request_slots};
// Test-only: the adopt's token → uuid memo outlives a call now, so it also
// outlives a test. Cleared with the endpoint overrides that make it reachable.
#[cfg(test)]
pub(crate) use scheduler::reset_identity_memo;
// The stored-token probe suppression in `oauth.rs` and its tests share the memo's
// token-hash key derivation.
pub(crate) use scheduler::identity_key;
// Test-only: point `/usage` at a loopback listener so `fetch_with_rotation`'s
// 401-then-rotate leg — and the refusal inside it — can run offline.
#[cfg(test)]
pub(crate) use fetch::{clear_usage_endpoint_override, set_usage_endpoint_override};
// The `/profile` TTL decision itself, re-exported for the account-swap tests in
// `actions`: asserting through the real decision proves a swap expired BOTH
// halves of the clock (memo + durable stamp), which no fixture of it would.
#[cfg(test)]
pub(crate) use fetch::take_profile_fetch;
