mod burn;
mod fetch;
mod scheduler;

pub(crate) use burn::{
    BURN_GAP_CUT_MS, BURN_LOOKBACK_MS, BURN_MIN_SAMPLES, compute_burn_rates_from_history,
    project_utilization,
};
#[allow(unused_imports)]
pub(crate) use fetch::{
    ANTHROPIC_ORIGIN, AccountIdentity, ExtraPeriod, ExtraUsage, IdentityProbe, LABEL_5H, LABEL_7D,
    LoginProfile, PlanInfo, PlanTier, ScopedWindow, SpendInfo, UsageInfo, UsageWindow,
    WindowDollars, await_request_slot, cli_user_agent, epoch_secs_to_iso, expire_profile_ttl,
    fetch_account_identity, fetch_account_uuid, five_hour_live, http_agent, humanize_duration,
    ideal_pace_pct, iso_to_epoch_secs, now_epoch_secs, now_ms, parse_retry_after,
    parse_retry_after_at, probe_account_identity, probe_login_profile, seed_login_anchor,
    seven_day_live, spent_resume_in_secs, window_avg_pace_per_day, windows_maxed,
};
pub(crate) use scheduler::{
    ActivityStore, CodexStandbyOutcome, FetchStatus, KickBlock, KickBlocks, LastFetchedAt,
    NextRefreshPerProfile, OpResult, OpResultReceiver, OpResultSender, Origin, PendingSwitch,
    PendingSwitchEntry, PendingSwitchOff, PollStreaks, ProfileActivity, RefetchQueue,
    StartupReceiver, StartupSender, StartupSignal, StatusStore, StreakCounts,
    SuppressedGenericStore, ThirdPartyList, ThirdPartyStatusStore, ThirdPartyUsageStore, TokenList,
    UsageStore, any_busy, bootstrap_fetch, bootstrap_third_party, clear_activity,
    codex_refresh_parked, collect_oauth_seed_names, collect_third_party_entries, collect_tokens,
    enqueue_pending_switch, is_idle, is_stuck_rate_limited, is_stuck_streak,
    kick_block_switch_grade, mark_activity, select_switch_winner, select_switch_winner_for,
    spawn_refresher, switch_gate_in_flight, switch_grade_kick_lifts,
};
// The active-cap boundary is only referenced by tests (production code reaches it
// through `is_stuck_rate_limited`); gate the re-export behind `cfg(test)` so it
// isn't a dead symbol in the shipped binary, while keeping the `stale`/distrust
// tests robust against a change to the constant's value.
#[cfg(test)]
pub(crate) use scheduler::ACTIVE_CAP_MAX_STREAK;
// Test-only: reset the per-host request-spacing slots so a real-bytes wire test
// driving a builder through `await_request_slot` doesn't sleep out the window.
#[cfg(test)]
pub(crate) use fetch::reset_request_slots;
// The `/profile` TTL decision itself, re-exported for the account-swap tests in
// `actions`: asserting through the real decision proves a swap expired BOTH
// halves of the clock (memo + durable stamp), which no fixture of it would.
#[cfg(test)]
pub(crate) use fetch::take_profile_fetch;
