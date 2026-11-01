mod auto_start_queue;
mod burn;
mod codex;
// The headers codex's own CLI sends on its WHAM endpoints, shared by the poll
// and the reset spend so the two cannot drift apart again.
mod codex_headers;
// `clauth limit-reset`: the list/consume pair, its selection rule and its text.
// One caller (`main.rs`), so reached by path rather than re-exported item by item.
pub(crate) mod codex_reset;
mod fetch;
mod forget;
// Usage read off an inference response's headers, one parse per provider, and
// the probe that asks for it.
mod inference;
// The auto-start kick sends the probe's request, so the two cannot drift apart.
pub(crate) use inference::anthropic::send as anthropic_send;
mod scheduler;
// The partition/tick pair is unreachable in production (`mod scheduler` is
// private and this re-export is `#[cfg(test)]`); tests in other modules drive
// it through here, so the shipped binary's reachability stays as the base had
// it (S5) — the same `pub(crate)` + private-mod + cfg(test) re-export pattern
// `scan_auto_switch` already uses.
#[cfg(test)]
pub(crate) use scheduler::{SchedulerState, tick};
// Gated like its one caller, the re-entered daemon child.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) use scheduler::CLAUTH_TEST_RUN_SCHEDULER;

pub(crate) use burn::{
    BURN_GAP_CUT_MS, BURN_LOOKBACK_MS, BURN_MIN_SAMPLES, WalletRate, WalletSample,
    compute_burn_rates_from_history, funded_wallet_rate, project_utilization,
};
pub(crate) use codex::fetch_codex_usage;
#[cfg(test)]
pub(crate) use codex::{clear_codex_usage_url_override, set_codex_usage_url_override};
// The mapping is exercised directly by the codex chain tests, which drive a real
// wham/usage body through the walk rather than hand-building a UsageInfo.
#[cfg(test)]
pub(crate) use codex::map_usage as map_codex_usage;
#[allow(unused_imports)]
pub(crate) use fetch::{
    ANTHROPIC_ORIGIN, AccountEmail, ExtraPeriod, ExtraUsage, LABEL_5H, LABEL_7D, LABEL_30D,
    LoginProfile, ORG_REFUSED_STATUS, PlanInfo, PlanTier, ProfileFetchDecision,
    ProfileUnauthorized, ScopedWindow, SpendInfo, USAGE_HTTP_DEADLINES, UsageInfo, UsageWindow,
    WindowDollars, await_request_slot, cli_user_agent, decide_profile_fetch, epoch_secs_to_iso,
    expire_profile_ttl, fetch_account_uuid, fetch_profile_plan, five_hour_live,
    forget_profile_memo, http_agent, humanize_duration, ideal_pace_pct, iso_to_epoch_secs,
    long_window_live, mark_profile_read_stale, ms_to_iso, no_five_hour_limit_cached,
    note_profile_memo, now_epoch_secs, now_ms, parse_retry_after, parse_retry_after_at,
    probe_account_uuid, probe_login_profile, reconcile_identity_anchor_to_held, seed_login_anchor,
    seed_plan_cache, spent_resume_in_secs, stamp_profile_fetch, window_avg_pace_per_day,
    window_duration_secs, windows_maxed,
};
pub(crate) use scheduler::{
    ActivityStore, DeadLoginStats, FetchLeg, FetchStatus, KickBlock, KickBlocks, LastFetchedAt,
    LegKey, NextRefreshPerProfile, OpResult, OpResultReceiver, OpResultSender, PendingSwitch,
    PendingSwitchOff, PendingSwitchTarget, PollStreaks, ProfileActivity, RefetchQueue, RotateDone,
    SkippedUnreadable, SpendMarker, StartupReceiver, StartupSender, StartupSignal, StatusStore,
    StreakCounts, SuppressedAuthExpiredStore, ThirdPartyBroken, ThirdPartyList,
    ThirdPartyStatusStore, ThirdPartyStreaks, ThirdPartyUsageStore, TokenList, UsageStore,
    WeeklyResetKicks, any_busy, any_fetch_leg_admits, any_fetch_leg_polls, bootstrap_fetch,
    bootstrap_third_party, build_token_entries, claim_marked_spend,
    claim_scheduler_spend_unless_shutting_down, clear_activity, collect_oauth_seed_names,
    collect_probe_seeds, collect_third_party_entries, collect_third_party_seed_names,
    collect_token_seeds, collect_tokens, current_key_rejected_names, dead_login_fingerprint,
    dead_login_is_setup_token, decision_fresh_any, display_activity, end_rotation,
    fill_chain_snapshot_stores, is_idle, is_stuck_rate_limited, is_stuck_streak,
    kick_block_switch_grade, mark_activity, mark_fetch_activity, note_isolated_credit_refusal,
    oauth_without_pair, polled_third_party_names, probes_switched_off,
    profile_credential_fingerprint, queued_switch_away_is_stale,
    raise_refreshing_unless_shutting_down, read_skipped_unreadable, reading_is_polled,
    reseed_keeping_cadence, seed_dead_logins, selected_activity, selected_next_refresh,
    spawn_refresher, spending_account, switch_gate_in_flight, switch_grade_kick_blocked_from_cache,
    switch_grade_kick_lifts, sync_dead_logins, third_party_credentialed, token_spend_in_flight,
    usage_cache_clock_of, usage_interval_ms, usage_poll_pair, usage_probe_admits,
};
// The queue's history-pair classifier stays module-private — reached by
// `usage::auto_start_queue`'s own tests through `super::` — while the gap arithmetic is
// re-exported: the scheduler sizes the gap itself, and `status_json` derives
// `next_open_at` through [`auto_start_queue::next_queue_open_secs`].
pub(crate) use auto_start_queue::{
    AutoStartQueueState, Candidate, QueueSlot, auto_start_queue_members, elect_queue_member,
    forget_election_failures, history_anchor, move_election_failures,
    new_state as new_auto_start_queue_state, next_queue_open_secs, note_queue_kick_failed,
    note_queue_open, queue_anchor, queue_anchor_cached, queue_due, queue_failures, queue_gap_secs,
    queue_slot, seed_queue_anchor,
};
// The active-cap boundary is only referenced by tests (production code reaches it
// through `is_stuck_rate_limited`); gate the re-export behind `cfg(test)` so it
// isn't a dead symbol in the shipped binary, while keeping the `stale`/distrust
// tests robust against a change to the constant's value.
#[cfg(test)]
pub(crate) use scheduler::ACTIVE_CAP_MAX_STREAK;
// Test-only: the daemon producer→drain integration drives `scan_auto_switch`
// (the pending-switch producer) directly, so a queued record's cause is the
// producer's own output rather than a hand-built one.
pub(crate) use scheduler::DEGRADED_GAP_CEILING_MS;
#[cfg(test)]
pub(crate) use scheduler::LAST_REFRESHER_SKIP_STORE;
pub(crate) use scheduler::MAX_RETRY_AFTER_MS;
#[cfg(test)]
pub(crate) use scheduler::clear_codex_compare_hooks_for_test;
#[cfg(test)]
pub(crate) use scheduler::clear_seed_hooks_for_test;
#[cfg(test)]
pub(crate) use scheduler::scan_auto_switch;
pub(crate) use scheduler::token_clock_expired;
#[cfg(test)]
pub(crate) use scheduler::{SeedPhase, set_seed_hook};
// Test-only: reset the per-host request-spacing slots so a real-bytes wire test
// driving a builder through `await_request_slot` doesn't sleep out the window,
// and read one back so a leg's reservation is assertable without that sleep.
#[cfg(test)]
pub(crate) use fetch::{
    reserved_request_slot, reset_profile_fetched_for_test, reset_request_slots, set_compare_hook,
};
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

pub(crate) use forget::{
    ForgetPlan, Half, MemoryStores, OAuthIdentity, SnapshotMap, apply_pending_forgets,
    endpoint_identity_plan, epoch_guard, epoch_guard_if_current, epoch_value, forget_account_disk,
    forget_disk, forget_disk_plan, forget_memory, forget_probe_reading, hash_field,
    inference_key_fingerprint, last_account, names_another_account, new_snapshot, note_forget,
    note_forget_plan, note_rename, oauth_changed, pairless_identity, profile_key_fingerprint,
    provider_bytes, read_before_identities, read_codex_for_compare, read_codex_identity,
    read_oauth_for_compare, read_third_party_for_compare, read_throughput_for_compare,
    register_memory_stores, reload_and_forget, setup_token_plan, spawn_oauth_identity,
    third_party_identity, throughput_for_endpoint, throughput_identity,
    throughput_identity_for_spawn,
};
#[cfg(test)]
pub(crate) use forget::{
    bump_epoch_for_test, reset_pending_forgets_for_test, try_lock_epoch_for_test,
};
