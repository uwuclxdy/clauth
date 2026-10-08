#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Characterization of the daemon's per-tick work (`Daemon::tick` and the
//! drains extracted to `src/daemon/tick.rs`) — the top reliability path.
//!
//! All disk state is redirected into a [`HomeSandbox`] tempdir, and
//! `keychain::enabled()` is false under `cfg(test)`, so the switch paths exercise
//! the file/symlink model only and NEVER touch the operator's real `~/.clauth`,
//! `~/.claude`, or the `Claude Code-credentials` Keychain item (Incident C
//! guardrail). No network: every OAuth token is minted with a future expiry so the
//! pre-install auth gate returns `Ready` without a refresh.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use crate::profile::{
    AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, claude_dir, clauth_dir,
    reload_fingerprint, save_app_state, save_profile,
};
use crate::testutil::{HomeSandbox, blank_profile, set_mtime, through_handle};
#[cfg(unix)]
use crate::testutil::{git_shim, heal_env, lightweight_tag, stateful_heal_shim};
use crate::usage::{
    FetchLeg, PendingSwitchTarget, ProfileActivity, mark_activity, mark_fetch_activity, now_ms,
};

use super::Daemon;

/// Queue a switch target on the daemon's pending set, with no key-rejection cause
/// (an ordinary exhaustion/home decision).
fn stage_switch(d: &Daemon, target: &str) {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .insert(PendingSwitchTarget {
            target: target.into(),
            key_rejected_cause: None,
        });
}

/// Queue a switch-away caused by the active's key rejection, recording the
/// fingerprint the decision was made under.
fn stage_key_rejected_switch(d: &Daemon, target: &str, active: &str, recorded_fp: u64) {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .insert(PendingSwitchTarget {
            target: target.into(),
            key_rejected_cause: Some((active.to_string(), recorded_fp)),
        });
}

/// Snapshot the queued switch targets (sorted), for asserting re-queue / clearing.
fn queued_targets(d: &Daemon) -> Vec<String> {
    let mut v: Vec<String> = d
        .pending_switch
        .lock()
        .expect("pending_switch")
        .iter()
        .map(|t| t.target.clone())
        .collect();
    v.sort();
    v
}

/// Epoch-ms an hour ahead — a token with real life left, so the auth gate takes
/// the no-refresh `Ready` path.
fn future_expiry() -> i64 {
    crate::usage::now_ms() as i64 + 3_600_000
}

/// Minimal OAuth credentials whose access token round-trips through the profile
/// store; `access` also seeds a distinct refresh token so profiles never collide.
fn oauth_creds(access: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("rt-{access}")),
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// A blank profile with a live-token credential block attached.
fn profile_with_creds(name: &str, access: &str) -> Profile {
    let mut p = blank_profile(&crate::profile::ProfileName::from(name));
    p.credentials = Some(oauth_creds(access));
    p
}

/// Persist `profiles` + an `AppState` (given active + refresh interval) to the
/// sandbox disk, then return the matching in-memory `AppConfig` the daemon owns.
fn persist(profiles: Vec<Profile>, active: Option<&str>, refresh_interval_ms: u64) -> AppConfig {
    let mut state = AppState {
        active_profile: active.map(Into::into),
        profiles: profiles.iter().map(|p| p.name.clone()).collect(),
        refresh_interval_ms,
        ..AppState::default()
    };
    // fallback_chain left empty — the drains under test don't consult it.
    state.fallback_chain.clear();
    for p in &profiles {
        save_profile(p).expect("persist profile");
    }
    save_app_state(&state).expect("persist app state");
    AppConfig { state, profiles }
}

/// Build a daemon over `config`, writing `status.json` beside the sandbox root.
fn daemon_for(config: AppConfig) -> Daemon {
    let status_path = clauth_dir().expect("clauth dir").join("status.json");
    Daemon::new(config, status_path)
}

/// Symlink `~/.claude/.credentials.json` at the profile's stored credentials so
/// the active link classifies as `LinkedTo` (clean — no unsaved divergence).
fn link_active_clean(name: &str) {
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from(name))
        .expect("link active credentials");
}

/// Write `~/.claude/.credentials.json` as a REGULAR file with an access token
/// that differs from `name`'s stored one — a genuine CC re-login the daemon must
/// treat as unsaved divergence (`active_diverged_unsaved` → true).
fn diverge_active(diff_access: &str) {
    let dir = claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir ~/.claude");
    let live = dir.join(".credentials.json");
    let bytes = serde_json::to_vec(&oauth_creds(diff_access)).expect("serialize live");
    std::fs::write(&live, bytes).expect("write live credentials");
}

fn active_of(d: &Daemon) -> Option<String> {
    d.config
        .lock()
        .expect("config")
        .state
        .active_profile
        .as_deref()
        .map(str::to_string)
}

// ── tick(): the extracted loop body ───────────────────────────────────────────

/// `tick` on an idle daemon with empty queues writes `status.json` and changes
/// nothing else — the pure no-op characterization of one loop iteration. Stays
/// cross-platform: the armed throttle is what keeps the tick's heal inert here,
/// since the gate's pointer read cannot be sandboxed on Windows.
#[test]
fn tick_with_empty_queues_writes_status_and_leaves_active_unchanged() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);
    let status_path = daemon.status_path.clone();

    daemon.tick();

    assert!(
        status_path.exists(),
        "tick must (re)write status.json each iteration"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "no queued switch → active profile is unchanged by a tick"
    );
}

/// The tick is a heal call site: one tick over a broken registration reaches
/// `claude` through the detached heal. Deleting the call from `tick` reds here,
/// which a healthy-registration variant could never do — that one is green
/// whether or not the tick calls anything. Unix-only: the fake `claude` is a
/// shell shim.
#[cfg(unix)]
#[test]
fn tick_heals_a_broken_plugin_registration() {
    use crate::testutil::{FakeClaude, join_background_tasks, seed_broken_plugin_registration};

    let home = HomeSandbox::new();
    let fake = FakeClaude::new(&home);
    crate::plugin_host::reset_heal_throttle_for_test();
    // The tick drives the herdr heal too; its throttle is armed so this test
    // stays spawn-free beside the claude heal it pins (the herdr heal's
    // fail-closed test assert needs the injected path, which no sandbox pins).
    crate::herdr::arm_heal_throttle_for_test();
    seed_broken_plugin_registration();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    daemon.tick();
    join_background_tasks();

    assert!(
        !fake.log().is_empty(),
        "one tick over a broken registration must reach the heal"
    );
}

/// The tick's herdr leg is the herdr-heal call site twin: one tick over a
/// stale registry reaches the fake herdr install when the saved `[update]`
/// toggle is on, and — after the tick's own reload picks a freshly persisted
/// `auto_update = false` up — spawns nothing, leaving the throttle floor
/// unclaimed (the re-enabled tick after it still installs).
#[cfg(unix)]
#[test]
fn tick_herdr_heal_follows_the_saved_update_toggle() {
    use std::ffi::OsStr;

    use crate::testutil::join_background_tasks;

    let home = HomeSandbox::new();
    let stale = crate::herdr::plugin_list_json(
        r#"{"enabled":true,"plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth","resolved_commit":"aaaaaaaaaaaaaaaa"}}"#,
    );
    let shim = stateful_heal_shim(home.home());
    git_shim(home.home());
    let tags = lightweight_tag("v0.15.1", "bbbbbbbbbbbbbbbb");
    let _env = heal_env(
        &home,
        &shim,
        &stale,
        &stale,
        &tags,
        &[("HERDR_SHIM_STATE", OsStr::new("1"))],
    );
    // The claude heal shares the tick; its throttle is armed so this test
    // stays spawn-free beside the herdr heal it pins.
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::reset_heal_throttle_for_test();

    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    // Saved off, written AFTER the daemon snapshot so the tick's reload is
    // what picks it up.
    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = false;
    save_app_state(&state).expect("persist off toggle");

    daemon.tick();
    join_background_tasks();
    assert!(
        !home.home().join("heal.log").exists(),
        "a tick after a reload with the saved toggle off reinstalls nothing"
    );

    // The saved-off tick must not have claimed the throttle: back on (again
    // through the tick's reload), the very next tick installs.
    let mut state = crate::profile::load_app_state().expect("load state");
    state.update.auto_update = true;
    save_app_state(&state).expect("persist on toggle");

    daemon.tick();
    join_background_tasks();
    let log = std::fs::read_to_string(home.home().join("heal.log")).unwrap_or_default();
    assert_eq!(
        log.trim(),
        "plugin install uwuclxdy/clauth/herdr-plugin --ref v0.15.1 --yes",
        "a tick with the saved toggle on reaches the fake install"
    );
}

// ── tick vs a wedged flock holder ─────────────────────────────────────────────

/// A tick draining both queues against a wedged flock holder completes within
/// the watchdog deadline: the first drain's wait spends the tick's shared
/// window, and the second drain is SKIPPED rather than handed a fresh wait —
/// pre-fix, two full waits (2 × 25 s) aborted the daemon mid-switch past the
/// 30 s watchdog. The switch is re-queued and the switch-off stays pending, so
/// the next tick retries both with a fresh window. Short seams pose the wedge:
/// the budget override shrinks the tick's window to 300 ms and the lock-timeout
/// override keeps a broken full wait observable in ~1 s instead of the real
/// 25 s.
#[test]
fn tick_skips_the_second_drain_once_a_wedged_flock_spends_the_budget() {
    let _home = HomeSandbox::new();
    crate::lock::set_subprocess_budget_override(Some(Duration::from_millis(300)));
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();

    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    // A second open file description holding the state flock — conflicts with
    // the daemon's acquisition exactly as a wedged peer would.
    let dir = clauth_dir().expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");

    let mut daemon = daemon_for(config);
    stage_switch(&daemon, "beta");
    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;

    let start = std::time::Instant::now();
    daemon.tick();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "the tick must complete despite the wedge (within the seam-posed deadline), took {elapsed:?}"
    );
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the wedged switch is re-queued for the next tick"
    );
    assert!(
        *daemon
            .pending_switch_off
            .lock()
            .expect("pending_switch_off"),
        "the skipped switch-off stays queued for the next tick (the pre-fix drain consumed \
         the flag before timing out)"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "no switch landed"
    );

    crate::lock::set_subprocess_budget_override(None);
    crate::lock::set_state_lock_timeout_override(None);
    drop(holder);
}

/// The healthy twin: with the flock free, a tick draining both queues runs
/// BOTH drains exactly as before the bound — the switch lands, then the
/// switch-off lands, and nothing is skipped or re-ordered. This pins the
/// byte-identical healthy path the aggregate bound must not disturb.
#[test]
fn tick_drains_both_queues_when_the_flock_is_free() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);
    stage_switch(&daemon, "beta");
    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;

    daemon.tick();

    assert_eq!(
        active_of(&daemon).as_deref(),
        None,
        "the switch AND the switch-off both landed"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the executed switch leaves nothing queued"
    );
    assert!(
        !*daemon
            .pending_switch_off
            .lock()
            .expect("pending_switch_off"),
        "the executed switch-off clears the flag"
    );
}

// ── drains_exhausted: the pure skip predicate ─────────────────────────────────

/// The pure skip predicate, both triggers: a spent tick window (`Some(0)`)
/// skips the next drain, and so does a spent watchdog deadline — whatever the
/// window holds. A tick with neither runs its next drain; `None` (no budget
/// armed, as in a direct drain call from a test) never skips on the window.
#[test]
fn drains_exhausted_names_both_skip_triggers() {
    let t = std::time::Instant::now();
    let deadline = t + Duration::from_secs(29);
    assert!(
        super::drains_exhausted(Some(Duration::ZERO), t, deadline),
        "a spent window skips the next drain"
    );
    assert!(
        !super::drains_exhausted(Some(Duration::from_secs(5)), t, deadline),
        "an unspent window before the deadline does not skip"
    );
    assert!(
        !super::drains_exhausted(None, t, deadline),
        "no budget armed (a direct drain call) does not skip"
    );
    assert!(
        super::drains_exhausted(Some(Duration::from_secs(5)), deadline, deadline),
        "a spent deadline skips whatever the window holds (now == deadline)"
    );
    assert!(
        super::drains_exhausted(
            Some(Duration::from_secs(5)),
            deadline + Duration::from_secs(1),
            deadline
        ),
        "a spent deadline skips whatever the window holds (now past the deadline)"
    );
}

// ── drain_pending_switch ──────────────────────────────────────────────────────

/// A queued auto-switch to an idle, installable target with a clean (non-diverged)
/// active is executed — active becomes the target.
#[test]
fn drain_pending_switch_executes_when_idle_and_clean() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "an idle, clean, installable target must be switched to"
    );
}

/// Q1 (repair direction): a queued switch-away whose record carries the
/// key-rejection cause is dropped when the active is re-keyed before dispatch —
/// the active stays and the stale decision is not re-queued.
#[test]
fn drain_pending_switch_drops_a_repaired_key_rejected_switch_away() {
    let _home = HomeSandbox::new();
    let tp = crate::profile::Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("old-key".to_string()),
    );
    let oauth = profile_with_creds("oauth", "at-oauth");
    let config = persist(vec![tp, oauth], Some("tp"), 90_000);
    let mut daemon = daemon_for(config);
    let old_fp = crate::usage::profile_credential_fingerprint(
        daemon
            .config
            .lock()
            .expect("config")
            .find(&crate::profile::ProfileName::from("tp"))
            .expect("tp present"),
    )
    .expect("credentialed");
    stage_key_rejected_switch(&daemon, "oauth", "tp", old_fp);

    // A repair lands before dispatch: the tp api key is re-keyed in place.
    daemon
        .config
        .lock()
        .expect("config")
        .find_mut(&crate::profile::ProfileName::from("tp"))
        .expect("tp present")
        .api_key = Some("new-key".to_string());

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("tp"),
        "the re-keyed active stays put"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the stale switch-away is dropped, not re-queued"
    );
}

/// Q1 (ordinary direction): a cause-absent ordinary exhaustion/home decision
/// still switches even beside a stale raw broken mark — a same-name re-key left
/// an inert old-key mark that must not drop the move.
#[test]
fn drain_pending_switch_still_switches_an_ordinary_decision_beside_a_stale_mark() {
    let _home = HomeSandbox::new();
    let tp = crate::profile::Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("old-key".to_string()),
    );
    let oauth = profile_with_creds("oauth", "at-oauth");
    let config = persist(vec![tp, oauth], Some("tp"), 90_000);
    let mut daemon = daemon_for(config);
    // A stale mark: recorded under a fingerprint that no longer matches tp's
    // current credential (the leftover a same-name re-key leaves behind).
    let stale_fp = crate::usage::profile_credential_fingerprint(&crate::profile::Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("other-key".to_string()),
    ))
    .expect("credentialed");
    daemon
        .third_party_broken
        .lock()
        .expect("broken")
        .insert("tp".to_string(), stale_fp);

    // The ordinary decision carries no cause.
    stage_switch(&daemon, "oauth");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("oauth"),
        "the ordinary exhaustion switch still executes beside the stale mark"
    );
}

/// Q1 producer→drain: a current key-rejection mark plus an independently
/// exhausted active must produce a cause-ABSENT record (the exhaustion alone
/// forces the move, so the scan — not a hand-built `stage_switch` — records no
/// cause), and a same-name OAuth conversion before dispatch must not drop it.
#[test]
fn drain_pending_switch_executes_a_producer_ordinary_record_after_same_name_conversion() {
    let _home = HomeSandbox::new();
    let tp = crate::profile::Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("key".to_string()),
    );
    let oauth = profile_with_creds("oauth", "at-oauth");
    let mut config = persist(vec![tp, oauth], Some("tp"), 90_000);
    config.state.fallback_chain = vec!["tp".into(), "oauth".into()];
    let mut daemon = daemon_for(config);

    // tp is key-rejected (a matching live mark) and independently exhausted;
    // oauth is clear + fresh, so the walk lands on it for exhaustion alone.
    let tp_fp = crate::usage::profile_credential_fingerprint(
        daemon
            .config
            .lock()
            .expect("config")
            .find(&crate::profile::ProfileName::from("tp"))
            .expect("tp present"),
    )
    .expect("credentialed");
    daemon
        .third_party_broken
        .lock()
        .expect("broken")
        .insert("tp".to_string(), tp_fp);
    let now = crate::usage::now_epoch_secs();
    let spent = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 100.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    let clear = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 10.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    daemon
        .usage_store
        .lock()
        .expect("usage")
        .insert("tp".to_string(), spent);
    daemon
        .usage_store
        .lock()
        .expect("usage")
        .insert("oauth".to_string(), clear);
    daemon
        .usage_status
        .lock()
        .expect("status")
        .insert("oauth".to_string(), crate::usage::FetchStatus::Fresh);

    // The producer queues the record; the test asserts the cause it records,
    // never hand-constructing a cause-absent record.
    crate::usage::scan_auto_switch(
        &daemon.config,
        &daemon.usage_store,
        &daemon.usage_status,
        &daemon.third_party_status,
        &daemon.third_party_streaks,
        &daemon.third_party_broken,
        &daemon.poll_streaks,
        &daemon.kick_blocks,
        &daemon.activity,
        &daemon.pending_switch,
        &daemon.pending_switch_off,
    );
    let queued: Vec<PendingSwitchTarget> = daemon
        .pending_switch
        .lock()
        .expect("pending")
        .iter()
        .cloned()
        .collect();
    assert_eq!(
        queued,
        vec![PendingSwitchTarget {
            target: "oauth".to_string(),
            key_rejected_cause: None,
        }],
        "the producer must queue a cause-absent record for an independently exhausted key-rejected active"
    );

    // Same-name OAuth conversion before dispatch leaves the old key-rejection
    // mark inert; the ordinary record must still execute.
    {
        let mut c = daemon.config.lock().expect("config");
        let tp = c
            .find_mut(&crate::profile::ProfileName::from("tp"))
            .expect("tp present");
        tp.base_url = None;
        tp.api_key = None;
    }

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("oauth"),
        "the producer ordinary switch still executes after the same-name conversion"
    );
}

/// Q1 producer→drain (whole-chain home): the active's key rejection suppresses
/// its `preferred_days` claim, so the scan walks to the sibling whose bare
/// `preferred` fallback flag fires, and the record carries the cause. Repairing
/// the active before dispatch drops that rejection-caused switch.
#[test]
fn drain_pending_switch_drops_a_day_claim_repair_after_key_rejection() {
    use chrono::Weekday::*;
    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new(
        "a".to_string(),
        Some("https://example.com".to_string()),
        Some("key".to_string()),
    );
    a.preferred_days = vec![Mon, Tue, Wed, Thu, Fri, Sat, Sun];
    let mut b = profile_with_creds("b", "at-b");
    b.preferred = true;
    let mut config = persist(vec![a, b], Some("a"), 90_000);
    config.state.fallback_chain = vec!["a".into(), "b".into()];
    let mut daemon = daemon_for(config);

    let a_fp = crate::usage::profile_credential_fingerprint(
        daemon
            .config
            .lock()
            .expect("config")
            .find(&crate::profile::ProfileName::from("a"))
            .expect("a present"),
    )
    .expect("credentialed");
    daemon
        .third_party_broken
        .lock()
        .expect("broken")
        .insert("a".to_string(), a_fp);

    // b clear + fresh so the walk lands on it; a holds no usage entry, so the
    // without-run reads it healthy and home.
    let now = crate::usage::now_epoch_secs();
    let clear = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 10.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    daemon
        .usage_store
        .lock()
        .expect("usage")
        .insert("b".to_string(), clear);
    daemon
        .usage_status
        .lock()
        .expect("status")
        .insert("b".to_string(), crate::usage::FetchStatus::Fresh);

    // The producer queues the record; the test asserts the cause it records,
    // never hand-constructing a cause-bearing record.
    crate::usage::scan_auto_switch(
        &daemon.config,
        &daemon.usage_store,
        &daemon.usage_status,
        &daemon.third_party_status,
        &daemon.third_party_streaks,
        &daemon.third_party_broken,
        &daemon.poll_streaks,
        &daemon.kick_blocks,
        &daemon.activity,
        &daemon.pending_switch,
        &daemon.pending_switch_off,
    );
    let queued: Vec<PendingSwitchTarget> = daemon
        .pending_switch
        .lock()
        .expect("pending")
        .iter()
        .cloned()
        .collect();
    assert_eq!(
        queued,
        vec![PendingSwitchTarget {
            target: "b".to_string(),
            key_rejected_cause: Some(("a".to_string(), a_fp)),
        }],
        "the producer must queue a cause-bearing record when the active's day claim alone reclaims home without rejection"
    );

    // A repair lands before dispatch: `a` is re-keyed in place, so the recorded
    // fingerprint no longer matches the current credential.
    daemon
        .config
        .lock()
        .expect("config")
        .find_mut(&crate::profile::ProfileName::from("a"))
        .expect("a present")
        .api_key = Some("new-key".to_string());

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("a"),
        "the repaired day-claim active stays put"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the rejection-caused switch-away is dropped, not re-queued"
    );
}

/// The same queued switch is SKIPPED when the outgoing active has unsaved,
/// diverged credentials (a CC re-login / token rotation) — the daemon cannot
/// prompt, so it leaves the active profile in place for the operator.
#[test]
fn drain_pending_switch_skips_on_active_divergence() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Live ~/.claude token differs from alpha's stored token → Diverged, and alpha
    // has stored creds so it is not a first-login adoption.
    diverge_active("at-alpha-ROTATED");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a diverged, unsaved active must block the switch (no daemon prompt)"
    );
    // Deferred, not dropped — the divergence may resolve, so it stays queued.
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "a switch blocked by divergence is re-queued for retry, not silently dropped"
    );
}

/// Claude Code's logged-out SHELL (both tokens blanked, `expiresAt: 0` — what
/// CC writes when its own refresh dies, keeping unrelated keys like
/// `mcpOAuth`) still classifies Diverged, but holds no login to protect. The
/// queued switch must PROCEED over it — deferring wedged every headless
/// switch behind a TUI decision about an empty file while running sessions
/// sat at "Login expired" (observed 2026-07-15).
#[test]
fn drain_pending_switch_proceeds_over_a_logged_out_shell() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    let dir = claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir ~/.claude");
    let live = dir.join(".credentials.json");
    std::fs::write(
        &live,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": "",
                "expiresAt": 0,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write live shell");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "a token-less shell must not block the switch"
    );
    // The shell was replaced by beta's stored login (symlink on unix, copy on
    // Windows — assert through the content, not the link type).
    let installed: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read installed live credentials");
    assert_eq!(
        installed.access_token(),
        Some("at-beta"),
        "the live slot now holds the target's stored login"
    );
    // The empty shell was never captured over the outgoing store.
    let alpha_store = crate::profile::profile_dir(&crate::profile::ProfileName::from("alpha"))
        .expect("alpha dir")
        .join("credentials.json");
    let stored: ClaudeCredentials =
        crate::profile::read_json_file(&alpha_store).expect("read alpha store");
    assert_eq!(
        stored.access_token(),
        Some("at-alpha"),
        "the shell's blank tokens must never overwrite the outgoing profile's stored login"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the executed switch leaves nothing queued"
    );
}

/// A clauth-owned symlink in the live slot is never "unsaved credentials":
/// capturing a long-lived `setup-token` sidecar for the ACTIVE profile flips its
/// install source from `credentials.json` to `session-token.json`, so the live
/// symlink — still pointing at the old `credentials.json` store — classifies
/// Diverged, yet re-pointing it on the next switch loses no login. The queued
/// switch must PROCEED; deferring failed every unattended switch "unsaved
/// credentials" until its retry TTL (observed live 2026-07-21 on the macOS fork).
#[cfg(unix)]
#[test]
fn drain_pending_switch_proceeds_over_a_stale_clauth_symlink() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // The live slot is clauth's own symlink into alpha's rotating store — clean.
    link_active_clean("alpha");
    // A long-lived session token appears for alpha (no refresh token → never
    // rotates), flipping its install source to session-token.json while the live
    // symlink still points at credentials.json — classify now reads Diverged
    // though the symlink holds nothing unsaved.
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat-alpha".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let alpha_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("alpha"))
        .expect("alpha dir");
    std::fs::write(
        alpha_dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("serialize sidecar"),
    )
    .expect("write session-token sidecar");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "a clauth-owned symlink holds nothing unsaved — the switch must proceed"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the executed switch leaves nothing queued"
    );
}

/// The macOS steady-state twin of the test above, and the round-2 finding: after
/// a switch, Claude Code rewrites the live slot as a REGULAR-FILE mirror of the
/// Keychain, clobbering the symlink. The sidecar flip then makes classify read
/// Diverged over that regular file — but its login is alpha's saved
/// `credentials.json`, so the queued switch must still PROCEED. A
/// symlink-identity exemption reads the regular file as unsaved and defers here;
/// the content-based `live_login_is_stored` clears it. No `#[cfg(unix)]` — a
/// regular-file mirror is exactly the shape a Linux CI can pin for macOS.
#[test]
fn drain_pending_switch_proceeds_over_a_macos_regular_file_mirror() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // CC's regular-file mirror: alpha's stored login, written as a plain file
    // (not our symlink), holding the SAME access token as alpha's credentials.json.
    let dir = claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir ~/.claude");
    std::fs::write(
        dir.join(".credentials.json"),
        serde_json::to_vec(&oauth_creds("at-alpha")).expect("serialize mirror"),
    )
    .expect("write regular-file mirror");
    // The sidecar flips alpha's install source to session-token.json; the mirror
    // now classifies Diverged though its login is fully saved.
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat-alpha".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let alpha_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("alpha"))
        .expect("alpha dir");
    std::fs::write(
        alpha_dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("serialize sidecar"),
    )
    .expect("write session-token sidecar");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "a regular-file mirror of a saved login holds nothing unsaved — the switch must proceed"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the executed switch leaves nothing queued"
    );
}

/// A live file that does not PARSE is not a shell — it may be a CC write in
/// progress, i.e. possibly a login. The divergence deferral stays armed for
/// it, exactly like a real diverged login.
#[test]
fn drain_pending_switch_still_defers_on_a_torn_live_file() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    let dir = claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir ~/.claude");
    std::fs::write(
        dir.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":""#,
    )
    .expect("write torn live file");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "an unreadable live file keeps the deferral (mid-write caution)"
    );
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the deferred switch stays queued for retry"
    );
}

/// A queued switch whose target no longer resolves (deleted out-of-process
/// after the enqueue — `clauth delete` can't purge this daemon's in-memory
/// queue) is DROPPED with a last_error, never attempted. Pre-fix, the drain
/// ran `switch_profile` on the ghost: `force_link` removed the live
/// credentials file BEFORE the existence check fired, the entry re-queued,
/// and the next tick's snapshot read the missing live file as "logged out" —
/// nulling the ACTIVE profile's stored credentials (2026-07-12 review).
#[test]
fn drain_pending_switch_drops_a_vanished_target() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "ghost");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "the active profile must be untouched"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "a vanished target is dropped, not re-queued — retrying can't resurrect it"
    );
    // The live credentials link must still resolve to alpha — the pre-fix bug
    // tore it down on the way to the too-late existence check.
    assert!(
        crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json")
            .exists(),
        "the live credentials file survives"
    );
    // Alpha's stored credentials survive on disk (the pre-fix second tick
    // nulled them via the logged-out misread).
    let stored = crate::profile::profile_dir(&crate::profile::ProfileName::from("alpha"))
        .unwrap()
        .join("credentials.json");
    assert!(stored.exists(), "alpha's stored credentials survive");
}

/// A switch to a target that is still mid-fetch can't execute this tick, but the
/// request is RE-QUEUED (not dropped after one attempt) and lands once the target
/// goes idle — the deferred-not-dropped contract (a switch during a fetch window
/// used to evaporate after the `{ok:true}` ack).
#[test]
fn busy_target_requeued_not_dropped() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    // User switch to beta arrives while beta is mid-fetch.
    stage_switch(&daemon, "beta");
    mark_activity(
        &daemon.activity,
        &crate::profile::ProfileName::from("beta"),
        ProfileActivity::Fetching,
    );
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a busy target cannot switch this tick"
    );
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the busy switch is re-queued, not dropped after one attempt"
    );

    // Fetch completes → the re-queued switch lands on the next tick. The fetch
    // leg is what `mark_activity(.., Fetching)` opened, so the leg's own
    // completion boundary is what closes it.
    mark_fetch_activity(
        &daemon.activity,
        &FetchLeg::OAuth.key(crate::profile::ProfileName::from("beta")),
        ProfileActivity::Idle,
    );
    daemon.drain_pending_switch();
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "once idle, the re-queued switch executes"
    );
}

// ── reload_if_changed ─────────────────────────────────────────────────────────

/// An external `profiles.toml` change (later mtime) is picked up: the config is
/// replaced and the refresh interval re-read.
#[test]
fn reload_if_changed_fires_on_external_mtime_change() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let mut daemon = daemon_for(config);

    let external = AppState {
        active_profile: Some("alpha".into()),
        profiles: vec!["alpha".into()],
        refresh_interval_ms: 45_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external app-state write");
    let state_path = clauth_dir().unwrap().join("profiles.toml");
    set_mtime(&state_path, SystemTime::now() + Duration::from_secs(5));

    daemon.reload_if_changed();

    assert_eq!(
        daemon.refresh_interval.load(Ordering::Relaxed),
        45_000,
        "an external state change with a newer mtime must be reloaded"
    );
    assert_eq!(
        reload_fingerprint(),
        daemon.last_reload_fp,
        "reload adopts the on-disk fingerprint so it won't reload its own read again"
    );
}

// ── cross-process RMW atomicity (self-adoption window) ────────────────────────

/// After a switch, the daemon's `last_reload_fp` equals the on-disk fingerprint — it
/// adopted its OWN write (captured while holding the flock), so `reload_if_changed`
/// is a no-op for the self-write, yet a later external write (newer mtime) still
/// triggers a reload — the no-self-adoption-window contract.
#[test]
fn rmw_switch_adopts_own_write_mtime_then_reloads_external() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    // The daemon adopted its own write's mtime (captured under the flock).
    assert_eq!(
        daemon.last_reload_fp,
        reload_fingerprint(),
        "daemon adopts its own switch write's fingerprint"
    );
    // A self-write must not look like an external change — reload is a no-op here.
    daemon.reload_if_changed();
    assert_eq!(
        daemon.refresh_interval.load(Ordering::Relaxed),
        90_000,
        "no external change → the daemon does not reload its own write"
    );

    // A genuine external write (newer mtime) is still picked up.
    let external = AppState {
        active_profile: Some("beta".into()),
        profiles: vec!["alpha".into(), "beta".into()],
        refresh_interval_ms: 30_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external write");
    let state_path = clauth_dir().unwrap().join("profiles.toml");
    set_mtime(&state_path, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();
    assert_eq!(
        daemon.refresh_interval.load(Ordering::Relaxed),
        30_000,
        "a later external write is still reloaded (no over-adoption)"
    );
}

// ── switch failure backoff + log dedup ────────────────────────────────────────

/// The backoff schedule: the first couple of failures retry immediately (so the
/// common brief-fetch case still lands the instant the target goes idle), then it
/// grows exponentially and caps.
#[test]
fn switch_backoff_ms_grows_exponentially_and_caps() {
    use super::switch_backoff_ms;
    assert_eq!(switch_backoff_ms(0), 0);
    assert_eq!(switch_backoff_ms(1), 0);
    assert_eq!(switch_backoff_ms(2), 0, "first attempts retry immediately");
    assert_eq!(switch_backoff_ms(3), 2_000);
    assert_eq!(switch_backoff_ms(4), 4_000);
    assert_eq!(switch_backoff_ms(5), 8_000);
    assert_eq!(switch_backoff_ms(50), 60_000, "capped at the ceiling");
}

/// A persistently-failing switch (target permanently mid-fetch) must NOT log/retry
/// 1/tick: the failure log is deduped (same reason → one emission) and backoff is
/// engaged. This is the anti-log-storm contract.
#[test]
fn switch_failure_backoff_dedups_log_over_many_ticks() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    // beta never goes idle → every attempt fails with the same reason.
    mark_activity(
        &daemon.activity,
        &crate::profile::ProfileName::from("beta"),
        ProfileActivity::Fetching,
    );

    for _ in 0..30 {
        daemon.drain_pending_switch();
    }

    assert!(
        daemon.switch_failure_logs <= 2,
        "a stuck switch dedups its log — got {} emissions over 30 ticks",
        daemon.switch_failure_logs
    );
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the stuck switch stays queued (re-queued within its TTL), not dropped"
    );
    assert!(
        daemon
            .switch_backoff
            .as_ref()
            .is_some_and(|b| b.target == "beta" && b.attempts >= 3),
        "backoff engaged for the repeatedly-failing target"
    );
}

// ── ~/.clauth 0700 enforcement ────────────────────────────────────────────────

/// A boot must tighten an existing world-traversable `~/.clauth` tree to 0o700
/// (older builds / a permissive umask could leave it 0o755) AND chmod the
/// launchd-created `daemon.log` (which lands ~0o644) to 0o600 to match SECURITY.md.
#[cfg(unix)]
#[test]
fn clauth_tree_migrated_to_0700_on_boot() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let clauth = clauth_dir().unwrap();
    let profiles = clauth.join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    // Simulate an older, world-traversable tree + a launchd-created 0o644 log.
    std::fs::set_permissions(&clauth, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&profiles, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = clauth.join("daemon.log");
    std::fs::write(&log, b"boot\n").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();

    super::migrate_clauth_perms_700(&clauth);

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&clauth), 0o700, "~/.clauth tightened to 0o700");
    assert_eq!(
        mode(&profiles),
        0o700,
        "~/.clauth/profiles tightened to 0o700"
    );
    assert_eq!(mode(&log), 0o600, "daemon.log tightened to 0o600");
}

/// The give-up TTL closes the retry loop even when the last backoff step
/// reaches past it: with backoff state whose `not_before` is beyond
/// `retry_until`, the next drain must give up (drop the target, clear the
/// backoff) — not keep requeueing until `not_before` finally elapses.
#[test]
fn backoff_gate_gives_up_when_the_retry_window_closes() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("home", "at-home"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("home"),
        90_000,
    );
    link_active_clean("home");
    let mut daemon = daemon_for(config);

    let now = now_ms();
    daemon.switch_backoff = Some(super::SwitchBackoff {
        target: "beta".into(),
        attempts: 9,
        // Capped backoff step reaches PAST the retry window's edge.
        not_before: now + 60_000,
        reason: "target is mid-fetch".into(),
        retry_until: now.saturating_sub(1),
    });
    stage_switch(&daemon, "beta");

    daemon.drain_pending_switch();

    assert!(
        daemon.switch_backoff.is_none(),
        "a closed retry window clears the backoff state"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "the expired target is dropped, not requeued"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("home"),
        "no switch is attempted past the window"
    );
}

// ── uncapped_spenders (boot-time warning's pure collection) ───────────────────

/// A disabled member is never spend-armed by the walk, so it must never be
/// named in the "can spend with no cap" warning — only a live, enabled
/// uncapped sibling should surface.
#[test]
fn uncapped_spenders_excludes_disabled_includes_enabled_sibling() {
    let mut disabled = blank_profile(&crate::profile::ProfileName::from("off"));
    disabled.max_auto_spend = Some(5.0);
    disabled.disabled = true;
    let mut enabled = blank_profile(&crate::profile::ProfileName::from("on"));
    enabled.max_auto_spend = Some(5.0);

    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["off".into(), "on".into()],
            spend_budget_switching: true,
            switch_off_when_budget_spent: false,
            ..AppState::default()
        },
        profiles: vec![disabled, enabled],
    };

    let names = super::uncapped_spenders(&config);
    assert!(
        !names.contains(&"off"),
        "a disabled member must never be named as an uncapped spender"
    );
    assert!(
        names.contains(&"on"),
        "an enabled uncapped sibling must still be named"
    );
}

// ── no_usage_source_members (boot-time warning's pure collection, #110) ───────

/// Only an enabled subscription member with no OAuth login is named: that is
/// the one shape no leg ever reads. A login, an api key, or a disabled flag
/// (the walk never switches to it) keeps a member out.
#[test]
fn no_usage_source_members_names_only_enabled_members_clauth_cannot_measure() {
    let name = |n: &str| crate::profile::ProfileName::from(n);
    let mut keyed = blank_profile(&name("keyed"));
    keyed.api_key = Some("sk-test".into());
    let mut off = blank_profile(&name("off"));
    off.disabled = true;
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec![
                "polled".into(),
                "st-a".into(),
                "keyed".into(),
                "off".into(),
                "st-b".into(),
            ],
            ..AppState::default()
        },
        profiles: vec![
            profile_with_creds("polled", "at-polled"),
            blank_profile(&name("st-a")),
            keyed,
            off,
            blank_profile(&name("st-b")),
        ],
    };
    assert_eq!(super::no_usage_source_members(&config), ["st-a", "st-b"]);
}

/// The warning reaches the log as one line naming every such member, and says
/// nothing when the chain has none.
#[test]
fn warn_if_chain_has_no_usage_source_logs_one_line_naming_them() {
    let name = |n: &str| crate::profile::ProfileName::from(n);
    let config = |chain: Vec<&str>| AppConfig {
        state: AppState {
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles: vec![
            profile_with_creds("polled", "at-polled"),
            blank_profile(&name("st-a")),
            blank_profile(&name("st-b")),
        ],
    };

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        super::warn_if_chain_has_no_usage_source(&config(vec!["polled", "st-a", "st-b"]));
    }
    assert_eq!(
        lines.snapshot(),
        [
            "clauth daemon: can't read the usage of st-a, st-b. once one is active, clauth \
             won't switch away from it on its own. add an oauth login with `clauth login \
             <name>` so clauth can read it"
        ],
    );

    let quiet = crate::logline::LogLines::new();
    {
        let _capture = quiet.capture_here();
        super::warn_if_chain_has_no_usage_source(&config(vec!["polled"]));
    }
    assert!(
        quiet.snapshot().is_empty(),
        "a chain clauth can measure in full logs nothing"
    );
}

/// The standby arm tightens the tree BEFORE it parks, never after the takeover.
/// launchd creates `daemon.log` at the umask (0o644) before exec and a park is
/// unbounded in time, so a walk deferred to the promotion leaves a
/// world-readable log naming accounts for the whole wait.
#[cfg(unix)]
#[test]
fn stand_by_tightens_the_tree_before_parking_not_after_promotion() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A loose dir standing in for the log: the pre-park walk tightens it.
    let loose = dir.join("loose");
    std::fs::create_dir_all(&loose).expect("mkdir loose");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Stand in for the running daemon so the claim below really parks.
    let held = crate::profile::open_state_file(&dir.join(super::LOCK_FILE)).expect("open lock");
    held.try_lock().expect("hold the singleton lock");

    let super::Claim::Standby(slot) = super::claim_singleton(&dir, true).expect("claim") else {
        panic!("the second instance takes the one standby slot");
    };
    let parked = std::thread::spawn({
        let dir = dir.clone();
        move || super::stand_by(&dir, slot)
    });

    let mode = |p: &std::path::Path| {
        std::fs::metadata(p)
            .expect("stat loose")
            .permissions()
            .mode()
            & 0o777
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while mode(&loose) != 0o700 {
        assert!(
            !parked.is_finished(),
            "stand_by returned instead of parking"
        );
        assert!(
            Instant::now() < deadline,
            "the tree stayed 0o755 across 5s of parking: the walk runs only after the promotion, \
             so a standby's whole wait sits in a world-readable tree"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // Holder exits → the standby takes over.
    drop(held);
    let promoted = parked
        .join()
        .expect("stand_by thread")
        .expect("the standby promotes once the holder exits");
    drop(promoted);
}

/// A `clauth daemon` that loses the singleton race must exit having touched
/// nothing shared. The pile-up in #57 was 25 of these, each having already run
/// the runtime GC and the tree-wide chmod walk against the live daemon's state
/// before parking forever.
#[test]
fn a_redundant_instance_exits_without_touching_the_shared_tree() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");

    // A runtime tree with no live session: `gc_stale_runtimes` deletes it.
    let ghost = dir.join("profiles").join("ghost").join("runtime");
    std::fs::create_dir_all(&ghost).expect("mkdir ghost runtime");
    // A loose dir: `migrate_clauth_perms_700` tightens it to 0o700.
    let loose = dir.join("loose");
    std::fs::create_dir_all(&loose).expect("mkdir loose");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    // Stand in for the running daemon: hold the singleton lock for the call.
    let held = crate::profile::open_state_file(&dir.join(super::LOCK_FILE)).expect("open lock");
    held.try_lock().expect("hold the singleton lock");

    super::serve(
        super::StartMode::ExitIfRunning,
        None,
        &super::api::tls::CertSource::Lego,
    )
    .expect("a redundant instance exits clean");

    assert!(
        ghost.exists(),
        "the redundant instance ran the runtime GC against the live daemon's tree"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&loose)
            .expect("stat loose")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "the redundant instance walked the tree's modes before exiting"
        );
    }
}

/// The default's redundant line names the holder's pid so a `ps` dump ties back
/// to it; `--standby` reports a full queue instead. Pins the operator-facing
/// wording (`serve` logs it and exits, which a test can't easily capture).
#[test]
fn redundant_reason_names_the_pid_for_the_default_and_the_queue_for_standby() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");

    // No pid sidecar staged: the default still reads "already running", pid unknown.
    let default = super::redundant_reason(super::StartMode::ExitIfRunning);
    assert!(
        default.starts_with("already running (pid "),
        "the default's redundant reason must read 'already running (pid …)', got {default:?}"
    );

    // With a pid stamped, it surfaces the number.
    std::fs::write(dir.join(super::PID_FILE), "4242\n").expect("stamp pid");
    let with_pid = super::redundant_reason(super::StartMode::ExitIfRunning);
    assert!(
        with_pid.contains("4242"),
        "the default's redundant reason must name the holder pid, got {with_pid:?}"
    );

    let standby = super::redundant_reason(super::StartMode::Standby);
    assert!(
        standby.contains("standby"),
        "the --standby redundant reason must mention the full queue, got {standby:?}"
    );
}

// ── stale-config persist gates (lock-race row 3) ─────────────────────────────

/// A queued switch whose target is deleted out-of-process AFTER the daemon's
/// in-memory config was loaded must be dropped on the fresh membership read,
/// not switched to. The pre-fix drain read the vanished guard off the in-memory
/// list, so the delete (landing between the tick's reload and the drain) was
/// invisible and `switch_profile` ran against a ghost.
#[test]
fn drain_pending_switch_drops_a_target_deleted_after_enqueue() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");

    let mut disk = crate::profile::load_config().expect("load disk config");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("beta"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("beta"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a deleted target must not be switched to"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "a deleted target is dropped, not re-queued"
    );
    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from("beta"))
            .expect("dir")
            .exists(),
        "the deleted target's directory must stay deleted"
    );
}

/// A switch's whole-state save must not re-list an unrelated profile deleted by
/// the CLI after the daemon's config was loaded. The switch still lands on the
/// (still-existing) target; only the deleted row stays gone.
#[test]
fn drain_pending_switch_does_not_resurrect_a_deleted_row() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
            profile_with_creds("gamma", "at-gamma"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");

    let mut disk = crate::profile::load_config().expect("load disk config");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("gamma"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("gamma"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "the switch to beta still lands"
    );
    let reloaded = crate::profile::load_config().expect("reload");
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("gamma"))
            .is_none(),
        "the deleted profile's row must not come back through the switch's state save"
    );
}

/// The wrap-off's whole-state save is the same shape as the switch's: it must
/// not re-list a profile deleted out from under the daemon while it turns
/// everything off.
#[test]
fn drain_pending_switch_off_does_not_resurrect_a_deleted_row() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("gamma", "at-gamma"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;

    let mut disk = crate::profile::load_config().expect("load disk config");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("gamma"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("gamma"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    daemon.drain_pending_switch_off();

    assert_eq!(
        active_of(&daemon).as_deref(),
        None,
        "the wrap-off turned everything off"
    );
    let reloaded = crate::profile::load_config().expect("reload");
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("gamma"))
            .is_none(),
        "the deleted profile's row must not come back through the switch-off state save"
    );
}

// ── CLAUTH_NO_API ───────────────────────────────────────────────────────────

/// The kill switch pinned at its CALL SITE, not just its predicate: with
/// `CLAUTH_NO_API=1` and a `--listen` address, `serve`'s listener decision must
/// yield the no-api arm — no certificate read, nothing for
/// `api::serve_prepared` to bind or import later. Deleting the `api_enabled()`
/// guard from the start path (leaving the predicate test green) re-arms a
/// listener the operator could only kill by editing the unit.
///
/// `Lego` (not a generated chain) and no `HomeSandbox` on purpose: under the
/// opt-out the decision never reads the certificate, so the arm is decided by
/// the env var alone. The pin's RED CHAIN is the certificate read: `prepare`
/// looks up this host's FQDN, then finds lego's directory through
/// `~/.clauth/tls.json` (which resolves `home_dir()`), so a regression that
/// deletes the guard dies at the sandbox panic — "test resolved the operator's
/// real home" — before any certificate file is opened, or at the `expect`
/// below when the FQDN lookup fails first. Both `assert`s never evaluate on
/// that edit; the panic is the red, and a legitimate one. A sandbox would
/// deadlock the guard another way: `HomeSandbox` holds `HOME_TEST_LOCK` for
/// the test's life and `with_no_api_env` takes it again.
#[test]
fn the_kill_switch_suppresses_the_listener_at_the_start_path() {
    with_no_api_env(Some("1"), || {
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let (prepared, no_api) =
            super::listener_setup(Some(addr), &super::api::tls::CertSource::Lego)
                .expect("the opt-out is a decision, not a failure");
        assert!(
            prepared.is_none(),
            "CLAUTH_NO_API=1 must suppress the listener at the start path"
        );
        assert_eq!(
            no_api,
            Some(addr),
            "the opt-out still names the address it declined to serve"
        );
    });
}

/// `set_var`/`remove_var` are unsafe in Rust 2024 because they aren't
/// thread-safe in a multi-threaded process. Serialized here by `HOME_TEST_LOCK`
/// (the one mutex every env mutator across the suite takes) and undone before
/// the closure returns, so no other thread observes a torn value.
fn with_no_api_env<F: FnOnce()>(val: Option<&str>, f: F) {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var(super::NO_API_ENV).ok();
    // SAFETY: test-only, serialized by the lock above, restored unconditionally.
    unsafe {
        match val {
            Some(v) => std::env::set_var(super::NO_API_ENV, v),
            None => std::env::remove_var(super::NO_API_ENV),
        }
    }
    f();
    // SAFETY: same as above.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(super::NO_API_ENV, v),
            None => std::env::remove_var(super::NO_API_ENV),
        }
    }
}

/// `CLAUTH_NO_API=1` and nothing else disables the listener.
///
/// The exact-`"1"` rule matters more here than for its siblings: this is the
/// kill switch an operator reaches for when a listening socket has to go and the
/// unit passing `--listen` cannot be edited. A build that also honoured `"true"`
/// or `"0"` would silently drop the listener for someone who set it to `0`
/// meaning "off, don't disable" — and the symptom is a remote client going dark,
/// not an error anywhere.
#[test]
fn the_rest_api_is_disabled_only_by_exactly_one() {
    with_no_api_env(None, || {
        assert!(super::api_enabled(), "unset → the listener is available");
    });
    with_no_api_env(Some("1"), || {
        assert!(!super::api_enabled(), "CLAUTH_NO_API=1 → no listener");
    });
    for other in ["0", "true", "yes", "", "11", " 1"] {
        with_no_api_env(Some(other), || {
            assert!(
                super::api_enabled(),
                "CLAUTH_NO_API={other:?} is not the opt-out spelling"
            );
        });
    }
}

// ── publish_status: the switch-side republish ────────────────────────────────

/// The feed currently sitting in the sandbox, as a `Value`.
fn feed_on_disk() -> serde_json::Value {
    let path = clauth_dir().expect("clauth dir").join("status.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("read status.json"))
        .expect("status.json is json")
}

/// Seed a feed the daemon could have written, naming `active` at `stamp`.
fn seed_feed(active: &str, stamp: &str) {
    let dir = clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    std::fs::write(
        dir.join("status.json"),
        format!(r#"{{"schema":1,"generated_at":"{stamp}","active_profile":"{active}","pending_switch":null,"wrap_off":false,"refresh_interval_ms":120000,"profiles":[]}}"#),
    )
    .expect("seed status.json");
}

/// A switch landing outside the daemon republishes the feed, but keeps the
/// daemon's last `generated_at`: readers (`clauth-tray`, the TUI's daemon chip)
/// treat a fresh stamp as proof a daemon is alive, and stamping `now` from the
/// CLI would forge that proof with no daemon running.
#[test]
fn a_non_daemon_publish_preserves_the_daemons_last_stamp() {
    let _home = HomeSandbox::new();
    let stamp = "2026-09-01T00:00:00+00:00";
    seed_feed("alpha", stamp);
    let config = persist(
        vec![
            profile_with_creds("alpha", "a-1"),
            profile_with_creds("beta", "b-1"),
        ],
        Some("beta"),
        120_000,
    );

    through_handle(config, super::publish_status);

    let body = feed_on_disk();
    assert_eq!(
        body["generated_at"],
        serde_json::json!(stamp),
        "the stamp is the daemon's last write, not this publish's"
    );
    assert_eq!(body["active_profile"], serde_json::json!("beta"));
}

/// With nothing to preserve (no daemon has ever published here), the switch-side
/// publish stamps the epoch rather than `now`: the file still names the account
/// the operator switched to, while the staleness rule still reads "no daemon".
#[test]
fn a_non_daemon_publish_with_no_prior_feed_stamps_the_epoch() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "a-1")],
        Some("alpha"),
        120_000,
    );

    through_handle(config, super::publish_status);

    let body = feed_on_disk();
    assert_eq!(
        body["generated_at"],
        serde_json::json!("1970-01-01T00:00:00+00:00")
    );
    assert_eq!(body["active_profile"], serde_json::json!("alpha"));
}

/// The daemon-owned form (the one the API's own switch republishes through)
/// stamps `now` even over an old file: a live daemon's write is itself the
/// freshness signal, so stamp preservation belongs to `publish_status` alone.
#[test]
fn a_direct_feed_write_stamps_now() {
    let _home = HomeSandbox::new();
    let stamp = "2026-09-01T00:00:00+00:00";
    seed_feed("alpha", stamp);
    let config = persist(
        vec![
            profile_with_creds("alpha", "a-1"),
            profile_with_creds("beta", "b-1"),
        ],
        Some("beta"),
        120_000,
    );

    super::write_status_feed(&config, None);

    let body = feed_on_disk();
    assert_ne!(
        body["generated_at"],
        serde_json::json!(stamp),
        "a daemon-side write is a freshness signal in itself"
    );
    assert_eq!(body["active_profile"], serde_json::json!("beta"));
}

#[test]
fn a_daemonless_publish_yields_to_a_feed_written_after_its_build_started() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            blank_profile(&crate::profile::ProfileName::from("a")),
            blank_profile(&crate::profile::ProfileName::from("b")),
        ],
        Some("b"),
        30_000,
    );

    let feed = clauth_dir().expect("feed dir").join("status.json");
    let incumbent: &[u8] = br#"{"sentinel": "incumbent"}"#;
    std::fs::write(&feed, incumbent).expect("seed incumbent feed");
    set_mtime(&feed, SystemTime::now() + Duration::from_secs(3600));

    let candidate: &[u8] = br#"{"active_profile": "b", "body": "candidate"}"#;
    super::publish_status_json_if_current(&config, candidate, SystemTime::now());
    assert_eq!(
        std::fs::read(&feed).expect("reread feed"),
        incumbent,
        "a feed written after the build started must survive the late commit"
    );

    set_mtime(&feed, SystemTime::now() - Duration::from_secs(3600));
    super::publish_status_json_if_current(&config, candidate, SystemTime::now());
    assert_eq!(
        std::fs::read(&feed).expect("reread feed"),
        candidate,
        "an older feed yields to the fresh body"
    );

    // The licensing rule is "stamped strictly before this build started": an
    // exactly-equal stamp must skip too (on a coarse-stamp filesystem an
    // equal stamp cannot prove the write preceded the build).
    let other: &[u8] = br#"{"sentinel": "second"}"#;
    std::fs::write(&feed, other).expect("reseed feed for the equality direction");
    let boundary = SystemTime::now();
    set_mtime(&feed, boundary);
    super::publish_status_json_if_current(&config, candidate, boundary);
    assert_eq!(
        std::fs::read(&feed).expect("reread feed"),
        other,
        "an exactly-equal stamp must skip, not publish"
    );
}

// ── the headless half of the day-list collision warning ────────────────────

/// The daemon runs with nobody watching a toast, so the collision has to reach
/// the log — and reach it once. `day_claim_notices` holds the messages, so a
/// tick that re-derives the same state is silent and a claimant change is not.
///
/// The capture is what pins the EMISSION: the gate transitions below would
/// read identically if `logline!` were dropped from the loop.
#[test]
fn the_daemon_logs_a_day_collision_once_per_change() {
    use chrono::Weekday::*;
    let _home = crate::testutil::HomeSandbox::new();

    let all = || vec![Mon, Tue, Wed, Thu, Fri, Sat, Sun];
    let mut a = blank_profile(&crate::profile::ProfileName::from("work"));
    a.preferred_days = all();
    let mut b = blank_profile(&crate::profile::ProfileName::from("personal"));
    b.preferred_days = all();

    let mut config = persist(vec![a, b], Some("work"), 60_000);
    config.state.fallback_chain = config.state.profiles.clone();
    let mut daemon = daemon_for(config);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    daemon.log_day_claim_notices();
    let first = daemon
        .day_claim_notices
        .first()
        .cloned()
        .expect("two claimants raise a notice");
    assert!(first.contains("2 accounts claim"), "got {first}");
    assert_eq!(
        lines.snapshot().len(),
        1,
        "the notice reaches the log, not just the gate: {:?}",
        lines.snapshot()
    );
    assert!(
        lines.snapshot()[0].contains(&first),
        "the line carries the notice: {:?}",
        lines.snapshot()
    );

    daemon.log_day_claim_notices();
    assert_eq!(
        daemon.day_claim_notices,
        vec![first.clone()],
        "an unchanged tick leaves the gate where it was"
    );
    assert_eq!(
        lines.snapshot().len(),
        1,
        "and writes no second line: {:?}",
        lines.snapshot()
    );

    {
        let mut cfg = daemon.config.lock().expect("config mutex poisoned");
        if let Some(p) = cfg.find_mut(&crate::profile::ProfileName::from("personal")) {
            p.preferred_days.clear();
        }
    }
    daemon.log_day_claim_notices();
    assert!(
        daemon.day_claim_notices.is_empty(),
        "the gate clears so a collision re-introduced logs again"
    );
    assert_eq!(
        lines.snapshot().len(),
        1,
        "clearing a notice says nothing: {:?}",
        lines.snapshot()
    );
}

/// N2 (D-arc): the status writer reads the SAME `third_party_streaks` Arc the
/// scheduler leg writes — `live_stores()` clones that Arc, never a fresh empty
/// store — so a deep streak the scheduler recorded moves the published `stale`.
#[test]
fn the_status_writer_reads_the_schedulers_own_streak_store() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["zai"]);
    let mut api = blank_profile(&crate::profile::ProfileName::from("zai"));
    api.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    api.api_key = Some("k".to_string());
    api.provider = crate::providers::Provider::from_base_url(api.base_url.as_deref().unwrap());
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![api],
    };
    let daemon = daemon_for(config);

    // The scheduler's own store (the Arc the refresher leg writes).
    daemon
        .third_party_streaks
        .lock()
        .unwrap()
        .insert("zai".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK + 1);
    daemon
        .third_party_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), crate::usage::FetchStatus::RateLimited);

    // The status writer's input: `live_stores()` clones the scheduler's Arc.
    let snapshot = daemon.live_stores().snapshot();
    let live = snapshot.signals();
    let cfg_snap = daemon.config.lock().unwrap().clone();
    let body = super::status_json::build_status(&cfg_snap, 300_000, Some(&live), false);
    let stale = body
        .profiles
        .iter()
        .find(|p| p.name.as_str() == "zai")
        .unwrap()
        .stale;
    assert!(
        stale,
        "the writer must read the scheduler's own streak store"
    );
}

// ── the TUI's `start daemon` ─────────────────────────────────────────────────

/// A stand-in `clauth`: prints what `spawn_detached` handed it, one fact per
/// line, to its stdout and a marker to its stderr.
#[cfg(unix)]
const SPAWN_PROBE: &str = r#"printf 'argv=%s\n' "$*"
printf 'cwd=%s\n' "$(pwd -P)"
printf 'leads=%s\n' "$( [ "$(ps -o pgid= -p $$ | tr -d ' ')" = "$$" ] && echo yes || echo no)"
printf 'claude=%s\n' "${CLAUDE_CONFIG_DIR-unset}"
printf 'codex=%s\n' "${CODEX_HOME-unset}"
echo stderr >&2"#;

/// The spawn runs `<exe> daemon` from `~/.clauth` in its own process group,
/// appends both streams to an owner-only `daemon.log`, and drops a session
/// home the caller inherited only when clauth built it.
#[cfg(unix)]
#[test]
fn start_runs_the_daemon_detached_into_its_log() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    let bin = tempfile::tempdir().expect("tempdir");
    let exe = crate::testutil::write_shim(bin.path(), "clauth", SPAWN_PROBE);
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    let runtime = clauth.join("profiles/p/runtime");
    let codex_home = clauth.join("profiles/p/codex-home");
    let log = clauth.join("daemon.log");
    let run = || {
        let status = super::spawn_detached(&exe)
            .expect("spawn")
            .wait()
            .expect("wait");
        assert!(status.success());
    };

    {
        let _env = crate::testutil::EnvPin::new(
            &home,
            &[
                ("CLAUDE_CONFIG_DIR", Some(runtime.as_os_str())),
                ("CODEX_HOME", Some(std::ffi::OsStr::new("/custom/codex"))),
            ],
        );
        run();
    }
    let mode = std::fs::metadata(&log).expect("log").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the log is owner-only");
    {
        let _env = crate::testutil::EnvPin::new(
            &home,
            &[
                (
                    "CLAUDE_CONFIG_DIR",
                    Some(std::ffi::OsStr::new("/custom/claude")),
                ),
                ("CODEX_HOME", Some(codex_home.as_os_str())),
            ],
        );
        run();
    }

    let cwd = clauth.canonicalize().expect("canonical clauth dir");
    let cwd = cwd.display();
    assert_eq!(
        std::fs::read_to_string(&log).expect("log"),
        format!(
            "argv=daemon\ncwd={cwd}\nleads=yes\nclaude=unset\ncodex=/custom/codex\nstderr\n\
             argv=daemon\ncwd={cwd}\nleads=yes\nclaude=/custom/claude\ncodex=unset\nstderr\n"
        ),
        "two starts append in order; each scrubs only the clauth-built home"
    );
}

#[cfg(unix)]
fn sh(script: &str) -> std::process::Child {
    std::process::Command::new("/bin/sh")
        .args(["-c", script])
        .spawn()
        .expect("spawn sh")
}

#[cfg(unix)]
#[test]
fn start_reports_a_child_that_exits_before_holding_the_lock() {
    let _home = HomeSandbox::new();
    let mut child = sh("exit 3");
    let outcome = super::await_start(
        &mut child,
        Duration::from_secs(5),
        Duration::from_millis(10),
    );
    assert_eq!(outcome, super::StartOutcome::Exited);
}

#[cfg(unix)]
#[test]
fn start_reports_a_held_singleton_as_up() {
    let _home = HomeSandbox::new();
    let _held = super::hold_daemon_lock();
    let mut child = sh("sleep 5");
    let outcome = super::await_start(
        &mut child,
        Duration::from_secs(5),
        Duration::from_millis(10),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(outcome, super::StartOutcome::Holding);
}

/// A child that lost the race to another daemon exits as redundant; the box
/// still has its daemon, so that reads as up, not as a failed start.
#[cfg(unix)]
#[test]
fn start_reads_a_child_that_lost_the_race_as_up() {
    let _home = HomeSandbox::new();
    let _held = super::hold_daemon_lock();
    let mut child = sh("exit 0");
    let exited = child.wait().expect("the child exits");
    assert!(exited.success());
    let outcome = super::await_start(
        &mut child,
        Duration::from_secs(5),
        Duration::from_millis(10),
    );
    assert_eq!(outcome, super::StartOutcome::Holding);
}

#[cfg(unix)]
#[test]
fn start_gives_up_at_the_wait() {
    let _home = HomeSandbox::new();
    let mut child = sh("sleep 5");
    let wait = Duration::from_millis(150);
    let started = std::time::Instant::now();
    let outcome = super::await_start(&mut child, wait, Duration::from_millis(10));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(outcome, super::StartOutcome::NotYet);
    assert!(started.elapsed() >= wait);
}

// ── headless bell ────────────────────────────────────────────────────────────

thread_local! {
    static RUNG: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn record_ring(template: &str, message: &str) -> anyhow::Result<()> {
    RUNG.with(|r| {
        r.borrow_mut()
            .push((template.to_string(), message.to_string()))
    });
    Ok(())
}

fn failing_ring(_template: &str, _message: &str) -> anyhow::Result<()> {
    RUNG.with(|r| r.borrow_mut().push(("failed".to_string(), String::new())));
    anyhow::bail!("starting bell_command program \"x\"")
}

fn rung() -> Vec<(String, String)> {
    RUNG.with(|r| r.borrow().clone())
}

/// A daemon whose `name` reads `util` 5h utilization from its disk cache under
/// a live `status`, with `bell_threshold` at `threshold` and `bell_command` at
/// `command`; its bell runs through `record_ring`.
fn bell_daemon(
    name: &str,
    threshold: f64,
    util: f64,
    status: crate::usage::FetchStatus,
    command: Option<&str>,
) -> Daemon {
    RUNG.with(|r| r.borrow_mut().clear());
    let mut profile = profile_with_creds(name, "at-bell");
    profile.bell_threshold = Some(threshold);
    let mut config = persist(vec![profile], Some(name), 60_000);
    config.state.bell_command = command.map(str::to_string);
    write_five_hour(name, util);
    let mut d = daemon_for(config);
    d.ring_bell = record_ring;
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert(name.to_string(), status);
    d
}

fn write_five_hour(name: &str, util: f64) {
    let resets_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let usage = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: util,
            resets_at: Some(resets_at),
        }),
        fetched_at: Some(now_ms()),
        ..Default::default()
    };
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::USAGE_CACHE_FILE,
        &usage,
    );
}

#[test]
fn a_fresh_crossing_rings_the_command_once_across_ticks() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("notify-send %s"),
    );
    d.write_status();
    d.write_status();
    assert_eq!(
        rung(),
        [(
            "notify-send %s".to_string(),
            "alert: alpha at 95%".to_string()
        )]
    );
}

#[test]
fn falling_below_re_arms_the_headless_bell() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("notify-send %s"),
    );
    d.write_status();
    write_five_hour("alpha", 10.0);
    d.write_status();
    write_five_hour("alpha", 93.0);
    d.write_status();
    assert_eq!(
        rung(),
        [
            (
                "notify-send %s".to_string(),
                "alert: alpha at 95%".to_string()
            ),
            (
                "notify-send %s".to_string(),
                "alert: alpha at 93%".to_string()
            ),
        ]
    );
}

#[test]
fn a_cached_reading_neither_rings_nor_clears_the_headless_bell() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Cached,
        Some("notify-send %s"),
    );
    d.write_status();
    assert_eq!(rung(), []);

    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
    d.write_status();
    write_five_hour("alpha", 5.0);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Cached);
    d.write_status();
    write_five_hour("alpha", 95.0);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
    d.write_status();
    assert_eq!(
        rung(),
        [(
            "notify-send %s".to_string(),
            "alert: alpha at 95%".to_string()
        )],
        "the stale 5% kept the bell, so the second fresh 95% stays silent"
    );
}

#[test]
fn no_bell_command_rings_nothing_and_leaves_the_latch() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon("alpha", 90.0, 95.0, crate::usage::FetchStatus::Fresh, None);
    d.write_status();
    assert_eq!(rung(), []);
    assert!(!d.bells.is_ringing("alpha"));
}

#[test]
fn unsetting_the_command_mid_crossing_keeps_the_rung_latch() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("notify-send %s"),
    );
    d.write_status();
    d.config.lock().expect("config").state.bell_command = None;
    write_five_hour("alpha", 5.0);
    d.write_status();
    assert!(
        d.bells.is_ringing("alpha"),
        "an unset command is inert, never a latch reset"
    );
}

#[cfg(unix)]
#[test]
fn a_hung_bell_program_never_stalls_the_status_write() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("sleep 10"),
    );
    d.ring_bell = crate::bell::run_command;
    let started = std::time::Instant::now();
    d.write_status();
    assert!(d.bells.is_ringing("alpha"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the status write waited on the bell program"
    );
}

#[test]
fn a_failed_bell_program_still_latches_the_crossing() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("x %s"),
    );
    d.ring_bell = failing_ring;
    d.write_status();
    d.write_status();
    assert_eq!(rung(), [("failed".to_string(), String::new())]);
}

#[test]
fn a_disabled_inactive_account_never_rings() {
    let _home = HomeSandbox::new();
    RUNG.with(|r| r.borrow_mut().clear());
    let mut alpha = profile_with_creds("alpha", "at-a");
    alpha.bell_threshold = Some(90.0);
    let mut beta = profile_with_creds("beta", "at-b");
    beta.bell_threshold = Some(90.0);
    beta.disabled = true;
    let mut config = persist(vec![alpha, beta], Some("alpha"), 60_000);
    config.state.bell_command = Some("notify-send %s".to_string());
    write_five_hour("alpha", 10.0);
    write_five_hour("beta", 99.0);
    let mut d = daemon_for(config);
    d.ring_bell = record_ring;
    for name in ["alpha", "beta"] {
        d.usage_status
            .lock()
            .expect("usage_status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }
    d.write_status();
    assert_eq!(rung(), []);
}

#[test]
fn an_aged_reading_under_a_fresh_status_does_not_ring() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("notify-send %s"),
    );
    let resets_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 95.0,
                resets_at: Some(resets_at),
            }),
            fetched_at: Some(now_ms() - 3_600_000),
            ..Default::default()
        },
    );
    d.write_status();
    assert_eq!(rung(), []);
}
