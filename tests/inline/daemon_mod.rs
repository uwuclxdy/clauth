#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
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

/// The OAuth access token the daemon's token list holds for `name`, if any.
fn daemon_token(d: &Daemon, name: &str) -> Option<String> {
    d.usage_tokens
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.name.as_str() == name)
        .map(|t| t.access_token.clone())
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

/// [`profile_with_creds`] whose access token expired a minute ago.
fn expired_profile_with_creds(name: &str, access: &str) -> Profile {
    let mut p = profile_with_creds(name, access);
    if let Some(oauth) = p
        .credentials
        .as_mut()
        .and_then(|c| c.claude_ai_oauth.as_mut())
    {
        oauth.expires_at = Some(now_ms() as i64 - 60_000);
    }
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

/// The drain marks the daemon's own skip store; the refresher's scan must read
/// the SAME `Arc`, or a mark is invisible to every scan and N4 returns while the
/// suite stays green. Pinned through the refresher's test-only recording seam.
#[test]
fn the_daemon_refresher_scans_the_same_skip_store_the_drain_marks() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let daemon = daemon_for(config);

    daemon.spawn_scheduler();

    let recorded = crate::usage::LAST_REFRESHER_SKIP_STORE
        .with(|slot| slot.borrow().clone())
        .expect("the refresher records the skip store it received");
    assert!(
        std::sync::Arc::ptr_eq(&recorded, &daemon.skipped_unreadable),
        "the refresher's scan must read the same store the drain marks"
    );
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

/// A forget noted without an immediate per-site drain still lands at the next
/// tick's safety-net apply, which this test pins.
#[test]
fn the_tick_applies_a_noted_forget() {
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
    daemon.usage_store.lock().expect("usage_store").insert(
        "alpha".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(
                    crate::usage::now_epoch_secs() + 3600,
                )),
            }),
            ..Default::default()
        },
    );
    daemon
        .usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);
    // Note the forget directly: no per-site drain runs before the tick, so only
    // the tick's apply can land it.
    crate::usage::note_forget(
        &crate::profile::ProfileName::from("alpha"),
        crate::usage::Half::OAuth,
    );

    daemon.tick();

    assert!(
        !daemon
            .usage_store
            .lock()
            .expect("usage_store")
            .contains_key("alpha"),
        "the tick's safety-net apply forgot the noted reading"
    );
    assert!(
        !daemon
            .usage_status
            .lock()
            .expect("usage_status")
            .contains_key("alpha"),
        "the tick's safety-net apply forgot the noted status"
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

/// The drain re-checks the queued target on disk and DROPS it when it holds no
/// login (no stored pair, no session-token sidecar, no api key): linking it
/// would sign the global slot out. Dropped, never re-queued — a later decision
/// re-derives the walk.
#[test]
fn drain_pending_switch_drops_a_target_holding_no_login() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            blank_profile(&crate::profile::ProfileName::from("beta")),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta");
    let lines = crate::logline::LogLines::new();
    let guard = lines.capture_here();
    daemon.drain_pending_switch();
    drop(guard);

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a target holding no login must never be linked"
    );
    assert!(
        daemon
            .pending_switch
            .lock()
            .expect("pending_switch")
            .is_empty(),
        "the login-less target is dropped, not re-queued"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: dropping queued switch to 'beta': it has no login or api key"
                .to_string()
        ],
        "the drop's log line is pinned by equality"
    );
}

/// The drain's re-check counts a session-token sidecar and an api key as a
/// login: a sidecar-only or key-only target still switches.
#[test]
fn drain_pending_switch_still_links_a_sidecar_only_or_key_only_target() {
    for (target, sidecar) in [("beta", true), ("gamma", false)] {
        let _home = HomeSandbox::new();
        let mut key_only = blank_profile(&crate::profile::ProfileName::from("gamma"));
        key_only.api_key = Some("sk-test".to_string());
        let config = persist(
            vec![
                profile_with_creds("alpha", "at-alpha"),
                blank_profile(&crate::profile::ProfileName::from("beta")),
                key_only,
            ],
            Some("alpha"),
            90_000,
        );
        if sidecar {
            crate::testutil::write_long_lived_sidecar(&crate::profile::ProfileName::from("beta"));
        }
        link_active_clean("alpha");
        let mut daemon = daemon_for(config);

        stage_switch(&daemon, target);
        daemon.drain_pending_switch();

        assert_eq!(
            active_of(&daemon).as_deref(),
            Some(target),
            "{target} holds a login, so the queued switch lands"
        );
    }
}

/// A target whose store exists but cannot be read (a `credentials.json` caught
/// mid-write, a `config.toml` hand-edited with a typo, a damaged
/// `session-token.json` on a sidecar-only member) proves nothing about its
/// login either way: the drain neither drops it as logged out nor links it on
/// a guess (linking a sidecar-only member past its corrupt sidecar installs
/// no login and signs the global slot out). It defers through the backoff,
/// re-reading the store on each retry.
#[test]
fn drain_pending_switch_defers_a_target_whose_stored_login_cannot_be_read() {
    let torn: [(&str, &str, &str); 3] = [
        (
            "delta",
            "session-token.json",
            "{\"claudeAiOauth\": {\"accessToken\": ",
        ),
        (
            "beta",
            "credentials.json",
            "{\"claudeAiOauth\": {\"accessToken\": ",
        ),
        (
            "gamma",
            "config.toml",
            "api_key = \"sk-test\"\nbase_url = [",
        ),
    ];
    for (target, file, bytes) in torn {
        let _home = HomeSandbox::new();
        let mut key_only = blank_profile(&crate::profile::ProfileName::from("gamma"));
        key_only.api_key = Some("sk-test".to_string());
        let config = persist(
            vec![
                profile_with_creds("alpha", "at-alpha"),
                profile_with_creds("beta", "at-beta"),
                key_only,
                blank_profile(&crate::profile::ProfileName::from("delta")),
            ],
            Some("alpha"),
            90_000,
        );
        link_active_clean("alpha");
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(target))
            .expect("profile dir");
        std::fs::write(dir.join(file), bytes).expect("write torn store");
        let mut daemon = daemon_for(config);

        stage_switch(&daemon, target);
        let lines = crate::logline::LogLines::new();
        let guard = lines.capture_here();
        daemon.drain_pending_switch();
        drop(guard);

        assert_eq!(
            active_of(&daemon).as_deref(),
            Some("alpha"),
            "a target whose {file} cannot be read is never linked"
        );
        assert_eq!(
            lines.snapshot(),
            vec![format!(
                "clauth daemon: deferring switch to '{target}': its stored login could not be read"
            )],
            "an unreadable {file} defers naming the unreadable store, pinned by equality"
        );
        assert_eq!(
            queued_targets(&daemon),
            vec![target.to_string()],
            "the deferred target is re-queued for a retry"
        );
    }
}

/// The deferral is bounded: a target whose store stays unreadable until its
/// retry window closes is given up naming that reason once and the skip,
/// never linked.
#[test]
fn drain_pending_switch_gives_up_on_a_store_that_stays_unreadable() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            blank_profile(&crate::profile::ProfileName::from("delta")),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("delta"))
        .expect("profile dir");
    std::fs::write(
        dir.join("session-token.json"),
        "{\"claudeAiOauth\": {\"accessToken\": ",
    )
    .expect("write torn sidecar");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "delta");
    let lines = crate::logline::LogLines::new();
    let guard = lines.capture_here();
    daemon.drain_pending_switch();
    // Close the retry window the first deferral opened.
    let now = now_ms();
    let backoff = daemon
        .switch_backoff
        .as_mut()
        .expect("the first drain defers through the backoff");
    backoff.not_before = now.saturating_sub(1);
    backoff.retry_until = now.saturating_sub(1);
    daemon.drain_pending_switch();
    drop(guard);

    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: deferring switch to 'delta': its stored login could not be read"
                .to_string(),
            "clauth daemon: gave up switching to 'delta': its stored login could not be read; \
             skipping it until it reads again"
                .to_string(),
        ],
        "the deferral, then the give-up naming the skip, the reason said once"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a store that never becomes readable is never linked"
    );
    assert!(
        queued_targets(&daemon).is_empty() && daemon.switch_backoff.is_none(),
        "the given-up target leaves neither a queued retry nor backoff state"
    );
}

/// A give-up on a store that reads unreadable names that store and the skip,
/// even when the deferral before it was a different reason: the line is the
/// only trace of why the member drops out of the walk.
#[test]
fn a_give_up_on_an_unreadable_store_names_it_after_a_mid_fetch_deferral() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("delta", "at-delta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let delta = crate::profile::ProfileName::from("delta");
    let creds = crate::profile::profile_dir(&delta)
        .expect("profile dir")
        .join("credentials.json");
    let readable = std::fs::read(&creds).expect("read stored credentials");
    let torn = "{\"claudeAiOauth\": {\"accessToken\": ";
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "delta");
    let lines = crate::logline::LogLines::new();
    let guard = lines.capture_here();
    std::fs::write(&creds, torn).expect("tear the store");
    daemon.drain_pending_switch();

    std::fs::write(&creds, &readable).expect("restore the store");
    mark_activity(&daemon.activity, &delta, ProfileActivity::Fetching);
    daemon
        .switch_backoff
        .as_mut()
        .expect("the first drain defers through the backoff")
        .not_before = now_ms().saturating_sub(1);
    daemon.drain_pending_switch();

    std::fs::write(&creds, torn).expect("tear the store again");
    let now = now_ms();
    let backoff = daemon
        .switch_backoff
        .as_mut()
        .expect("the mid-fetch drain keeps the backoff");
    backoff.not_before = now.saturating_sub(1);
    backoff.retry_until = now.saturating_sub(1);
    daemon.drain_pending_switch();
    drop(guard);

    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: deferring switch to 'delta': its stored login could not be read"
                .to_string(),
            "clauth daemon: deferring switch to 'delta': target is mid-fetch".to_string(),
            "clauth daemon: gave up switching to 'delta': its stored login could not be read \
             (last deferral: target is mid-fetch); skipping it until it reads again"
                .to_string(),
        ],
        "the give-up names the unreadable store, the last deferral and the skip"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "an unreadable store is never linked"
    );
    assert!(
        daemon
            .skipped_unreadable
            .lock()
            .expect("skipped set")
            .contains("delta"),
        "the given-up member is skipped until its store reads again"
    );
}

/// The retry window closes on its own clock: draining an unreadable target
/// again and again past each `not_before` keeps the window the first deferral
/// opened (one `deferring` line, `retry_until` never re-stamped), so the
/// give-up arrives when that window ends, not never.
#[test]
fn an_unreadable_targets_retry_window_is_never_restamped() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            blank_profile(&crate::profile::ProfileName::from("delta")),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("delta"))
        .expect("profile dir");
    std::fs::write(
        dir.join("session-token.json"),
        "{\"claudeAiOauth\": {\"accessToken\": ",
    )
    .expect("write torn sidecar");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "delta");
    let lines = crate::logline::LogLines::new();
    let guard = lines.capture_here();
    daemon.drain_pending_switch();
    let first_window = daemon
        .switch_backoff
        .as_ref()
        .expect("the first drain defers through the backoff")
        .retry_until;
    for pass in 0..3 {
        daemon
            .switch_backoff
            .as_mut()
            .expect("the deferral keeps its backoff state")
            .not_before = now_ms().saturating_sub(1);
        daemon.drain_pending_switch();
        assert_eq!(
            daemon
                .switch_backoff
                .as_ref()
                .expect("still inside the window")
                .retry_until,
            first_window,
            "pass {pass}: a retry past `not_before` keeps the first deferral's window"
        );
    }
    drop(guard);

    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: deferring switch to 'delta': its stored login could not be read"
                .to_string()
        ],
        "one deferral line for the whole window, however many retries"
    );
}

/// Inside an open backoff window an unreadable target re-queues quietly, like
/// every other deferral: the backoff gate runs before the unreadable check, so
/// neither the attempt count nor `not_before` moves.
#[test]
fn an_unreadable_target_inside_its_backoff_window_requeues_quietly() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            blank_profile(&crate::profile::ProfileName::from("delta")),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("delta"))
        .expect("profile dir");
    std::fs::write(
        dir.join("session-token.json"),
        "{\"claudeAiOauth\": {\"accessToken\": ",
    )
    .expect("write torn sidecar");
    let mut daemon = daemon_for(config);
    let now = now_ms();
    daemon.switch_backoff = Some(super::SwitchBackoff {
        target: "delta".into(),
        attempts: 1,
        not_before: now + 60_000,
        reason: "its stored login could not be read".into(),
        retry_until: now + 120_000,
    });

    for _ in 0..2 {
        stage_switch(&daemon, "delta");
        daemon.drain_pending_switch();
    }

    let b = daemon.switch_backoff.as_ref().expect("backoff kept");
    assert_eq!(
        (b.attempts, b.not_before),
        (1, now + 60_000),
        "inside `not_before` the backoff gate re-queues without a new attempt"
    );
    assert_eq!(queued_targets(&daemon), vec!["delta".to_string()]);
}

fn scan_once(d: &Daemon) {
    crate::usage::scan_auto_switch(
        &d.config,
        &d.usage_store,
        &d.third_party_usage_store,
        &d.usage_status,
        &d.third_party_status,
        &d.third_party_streaks,
        &d.third_party_broken,
        &d.skipped_unreadable,
        &d.poll_streaks,
        &d.kick_blocks,
        &d.activity,
        &d.pending_switch,
        &d.pending_switch_off,
    );
}

/// A member the drain gave up on because its stored login stays unreadable is
/// passed over by the next scan, which walks on to the next clear member;
/// once its store reads again it rejoins the walk. Without the skip the scan
/// re-picks it off the stale in-memory config (a store that cannot be parsed
/// fails every config reload) and the chain never moves past it.
#[test]
fn a_member_given_up_as_unreadable_is_passed_over_until_its_store_reads_again() {
    let _home = HomeSandbox::new();
    let mut config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("gamma", "at-gamma"),
            profile_with_creds("delta", "at-delta"),
        ],
        Some("alpha"),
        90_000,
    );
    config.state.fallback_chain = vec!["alpha".into(), "gamma".into(), "delta".into()];
    save_app_state(&config.state).expect("persist the chain");
    link_active_clean("alpha");
    let gamma_store = crate::profile::profile_dir(&crate::profile::ProfileName::from("gamma"))
        .expect("profile dir")
        .join("credentials.json");
    let readable = std::fs::read(&gamma_store).expect("read gamma's store");
    std::fs::write(&gamma_store, "{\"claudeAiOauth\": {\"accessToken\": ")
        .expect("tear gamma's store");
    let mut daemon = daemon_for(config);

    let now = crate::usage::now_epoch_secs();
    let window = |utilization: f64| crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    for (name, utilization) in [("alpha", 100.0), ("gamma", 10.0), ("delta", 10.0)] {
        daemon
            .usage_store
            .lock()
            .expect("usage")
            .insert(name.to_string(), window(utilization));
        daemon
            .usage_status
            .lock()
            .expect("status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }

    scan_once(&daemon);
    assert_eq!(
        queued_targets(&daemon),
        vec!["gamma".to_string()],
        "precondition: gamma is the walk's first clear pick"
    );
    daemon.drain_pending_switch();
    let closed = now_ms().saturating_sub(1);
    let backoff = daemon
        .switch_backoff
        .as_mut()
        .expect("gamma's unreadable store defers");
    backoff.not_before = closed;
    backoff.retry_until = closed;
    daemon.drain_pending_switch();
    assert!(
        queued_targets(&daemon).is_empty(),
        "precondition: the drain gave gamma up"
    );

    scan_once(&daemon);
    assert_eq!(
        queued_targets(&daemon),
        vec!["delta".to_string()],
        "the next scan passes over the given-up member and walks on"
    );

    daemon.pending_switch.lock().expect("pending").clear();
    std::fs::write(&gamma_store, &readable).expect("heal gamma's store");
    scan_once(&daemon);
    assert_eq!(
        queued_targets(&daemon),
        vec!["gamma".to_string()],
        "a store that reads again rejoins the walk"
    );
    assert_eq!(active_of(&daemon).as_deref(), Some("alpha"));
}

/// The daemon's chain walk reads the dead-key records: a key the provider
/// refused before the daemon started is skipped by its first scan, and one
/// another process records after it started (a delegate's 401) is skipped
/// once the next tick re-syncs — with no config change in between.
#[test]
fn the_daemons_chain_walk_skips_a_member_whose_dead_key_is_recorded_on_disk() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let keyed = |n: &str| {
        Profile::new(
            n.to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            Some(format!("sk-{n}")),
        )
    };
    let mut config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            keyed("gamma"),
            keyed("omega"),
            profile_with_creds("delta", "at-delta"),
        ],
        Some("alpha"),
        90_000,
    );
    config.state.fallback_chain = vec![
        "alpha".into(),
        "gamma".into(),
        "omega".into(),
        "delta".into(),
    ];
    save_app_state(&config.state).expect("persist the chain");
    link_active_clean("alpha");
    let record_dead_key = |n: &str| {
        let p = config
            .find(&crate::profile::ProfileName::from(n))
            .expect("member present");
        assert_eq!(
            p.provider,
            Some(crate::providers::Provider::DeepSeek),
            "fixture control: {n} is a typed key provider"
        );
        let key = crate::usage::profile_key_fingerprint(p).expect("a keyed member");
        crate::profile_cache::write_dead_key(&p.name, key);
    };
    record_dead_key("gamma");
    let mut daemon = daemon_for(config.clone());

    let now = crate::usage::now_epoch_secs();
    let window = |utilization: f64| crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    let clear_bars = crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![crate::providers::UsageBar {
            label: "5h".to_string(),
            pct: 10.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
            used: None,
            total: None,
            inference_quota: false,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    for (name, utilization) in [("alpha", 100.0), ("delta", 10.0)] {
        daemon
            .usage_store
            .lock()
            .expect("usage")
            .insert(name.to_string(), window(utilization));
        daemon
            .usage_status
            .lock()
            .expect("status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }
    for name in ["gamma", "omega"] {
        daemon
            .third_party_usage_store
            .lock()
            .expect("third-party usage")
            .insert(name.to_string(), clear_bars.clone());
        daemon
            .third_party_status
            .lock()
            .expect("third-party status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }

    scan_once(&daemon);
    assert_eq!(
        queued_targets(&daemon),
        vec!["omega".to_string()],
        "a dead key recorded before the daemon started is skipped by its first scan"
    );

    daemon.pending_switch.lock().expect("pending").clear();
    record_dead_key("omega");
    daemon.tick();
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "fixture control: the tick switched nothing"
    );
    scan_once(&daemon);
    assert_eq!(
        queued_targets(&daemon),
        vec!["delta".to_string()],
        "a dead key recorded after the daemon started is skipped once the next tick re-syncs"
    );
}

/// The daemon switch drain strips the outgoing identity block through its own
/// post-flock site. Deleting that one call must red this test and not the
/// manual-switch tests.
#[test]
fn drain_pending_switch_strips_the_outgoing_oauth_account() {
    let _home = HomeSandbox::new();
    let home_json = crate::profile::home_dir().unwrap().join(".claude.json");
    std::fs::write(
        &home_json,
        serde_json::to_vec_pretty(&serde_json::json!({
            "oauthAccount": {"emailAddress": "stale@x"},
            "numStartups": 7,
        }))
        .unwrap(),
    )
    .expect("write home .claude.json");

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
        "the switch executed"
    );
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&home_json).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "the daemon switch drain must strip the outgoing identity block"
    );
}

/// The daemon wrap-off drain strips the outgoing identity block through its own
/// post-flock site. Deleting that one call must red this test and not the
/// switch drain's.
#[test]
fn drain_pending_switch_off_strips_the_outgoing_oauth_account() {
    let _home = HomeSandbox::new();
    let home_json = crate::profile::home_dir().unwrap().join(".claude.json");
    std::fs::write(
        &home_json,
        serde_json::to_vec_pretty(&serde_json::json!({
            "oauthAccount": {"emailAddress": "stale@x"},
            "numStartups": 7,
        }))
        .unwrap(),
    )
    .expect("write home .claude.json");

    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;
    daemon.drain_pending_switch_off();

    assert_eq!(
        active_of(&daemon).as_deref(),
        None,
        "the wrap-off turned everything off"
    );
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&home_json).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "the daemon wrap-off drain must strip the outgoing identity block"
    );
}

/// The daemon tick honours a pending strip with no `clauth start` wrapper
/// running — a deferral lands while the daemon runs. Try-once, never waits: the
/// tick is the wrapper-free retry path.
#[test]
fn tick_honours_a_pending_strip_with_no_wrapper_running() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();

    let home_json = crate::profile::home_dir().unwrap().join(".claude.json");
    std::fs::create_dir_all(home_json.parent().unwrap()).unwrap();
    std::fs::write(
        &home_json,
        serde_json::to_vec_pretty(&serde_json::json!({
            "oauthAccount": {"emailAddress": "stale@x"},
            "numStartups": 7,
        }))
        .unwrap(),
    )
    .unwrap();

    // The durable marker a deferred strip left.
    let clauth = clauth_dir().unwrap();
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(clauth.join(".oauth-strip-pending"), b"stamp").unwrap();

    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    daemon.tick();

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&home_json).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "the daemon tick must honour a pending strip with no wrapper running"
    );
    assert!(
        !clauth.join(".oauth-strip-pending").exists(),
        "the honoured marker must clear"
    );
}

/// A change site the daemon tick runs — the adopt of a first login during a
/// switch — forgets the displaced OAuth reading right after the switch's locks
/// drop, the same call the TUI makes.
#[test]
fn a_daemon_switch_adopt_forgets_the_displaced_reading() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            blank_profile(&crate::profile::ProfileName::from("alpha")),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // The live slot holds a login the credential-less alpha never stored: a
    // first login the switch adopts (and forgets the old reading for).
    diverge_active("at-live");
    let mut daemon = daemon_for(config);
    daemon.usage_store.lock().expect("usage_store").insert(
        "alpha".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(
                    crate::usage::now_epoch_secs() + 3600,
                )),
            }),
            ..Default::default()
        },
    );
    daemon
        .usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "the switch executed"
    );
    assert!(
        !daemon
            .usage_store
            .lock()
            .expect("usage_store")
            .contains_key("alpha"),
        "the adopted account's reading is forgotten in the daemon"
    );
    assert!(
        !daemon
            .usage_status
            .lock()
            .expect("usage_status")
            .contains_key("alpha"),
        "the adopted account's status is forgotten in the daemon"
    );
}

/// A daemon switch whose install fails AFTER the adopt captured the live
/// login still forgets the adopted reading — the Err arm applies what the
/// change site noted before it failed.
#[test]
fn a_failed_daemon_switch_still_forgets_the_adopted_reading() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            blank_profile(&crate::profile::ProfileName::from("alpha")),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    diverge_active("at-live");
    let mut daemon = daemon_for(config);
    daemon.usage_store.lock().expect("usage_store").insert(
        "alpha".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(
                    crate::usage::now_epoch_secs() + 3600,
                )),
            }),
            ..Default::default()
        },
    );
    daemon
        .usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);

    // Fail the switch's finish half after the adopt captured the live login: a
    // directory at ~/.claude/settings.json refuses the merge-read inside
    // `apply_profile_to_claude_settings`, deterministic and root-safe.
    let settings = claude_dir().expect("claude dir").join("settings.json");
    std::fs::create_dir_all(&settings).expect("settings.json as dir");

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "the switch failed whole, the active marker is unmoved"
    );
    assert!(
        !daemon
            .usage_store
            .lock()
            .expect("usage_store")
            .contains_key("alpha"),
        "the adopted account's reading is forgotten even though the switch failed"
    );
    assert!(
        !daemon
            .usage_status
            .lock()
            .expect("usage_status")
            .contains_key("alpha"),
        "the adopted account's status is forgotten even though the switch failed"
    );
}

/// A re-login landing while the drain's gate refreshes the target, into a
/// login that is itself expiring: the switch defers for a retry, and the log
/// line carries the login-change sentence, never "refresh failed".
#[test]
fn drain_pending_switch_defers_a_target_relogged_into_an_expiring_login() {
    let home = HomeSandbox::new();
    let beta = expired_profile_with_creds("beta", "at-beta");
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha"), beta],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let (base, server) = crate::testutil::serve_endpoints(1, |_, _| {
        let mut cfg = crate::profile::load_config().expect("load config");
        crate::actions::overwrite_captured_profile(
            &mut cfg,
            &crate::profile::ProfileName::from("beta"),
            &crate::testutil::profile_id("beta"),
            crate::actions::CaptureSnapshot {
                credentials: Some(ClaudeCredentials {
                    claude_ai_oauth: Some(OAuthToken {
                        access_token: "at-relogin".to_string(),
                        refresh_token: Some("rt-relogin".to_string()),
                        expires_at: Some(now_ms() as i64 - 60_000),
                        scopes: None,
                        subscription_type: None,
                        ..crate::profile::OAuthToken::default_extra()
                    }),
                }),
                base_url: None,
                api_key: None,
                anchor: crate::actions::AnchorAction::Unproven,
                plan: None,
            },
        )
        .expect("install the re-login mid-refresh");
        (
            200,
            r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#.to_string(),
        )
    });
    let endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut daemon = daemon_for(config);
    stage_switch(&daemon, "beta");
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    daemon.drain_pending_switch();

    drop(capture);
    drop(endpoints);
    assert_eq!(
        server.join().expect("listener"),
        vec!["/v1/oauth/token".to_string()],
        "proof of execution: the gate refreshed the old login once"
    );
    assert_eq!(active_of(&daemon).as_deref(), Some("alpha"));
    assert_eq!(queued_targets(&daemon), vec!["beta".to_string()]);
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth: 'beta': the store holds a different login now; kept it and dropped the \
             refreshed pair"
                .to_string(),
            "clauth daemon: deferring switch to 'beta': 'beta' login changed during its refresh \
             and the new login expires soon. Retry in a moment"
                .to_string(),
        ]
    );
}

/// A queued switch whose target needs its expiring login refreshed reaches the
/// AUTH-1 gate only while the daemon is not stopping: once the shutdown flag is
/// up the gate's token spend is refused, so no request leaves the process and
/// the switch stays queued rather than being dropped or failed.
#[test]
fn drain_pending_switch_leaves_an_expiring_target_queued_while_stopping() {
    let home = HomeSandbox::new();
    let beta = expired_profile_with_creds("beta", "at-beta");
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha"), beta],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    // Zero requests expected: reaching the listener at all is the failure, so
    // the stub takes one more than a refusing run would ever send.
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (
            200,
            r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#.to_string(),
        )
    });
    let endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut daemon = daemon_for(config);
    daemon.shutting_down.store(true, Ordering::SeqCst);
    stage_switch(&daemon, "beta");

    daemon.drain_pending_switch();

    drop(endpoints);
    assert_eq!(
        server.join().expect("listener"),
        Vec::<String>::new(),
        "a stopping daemon sends no token request"
    );
    assert_eq!(active_of(&daemon).as_deref(), Some("alpha"));
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the refused switch stays queued, neither dropped nor failed"
    );
    assert!(
        daemon.switch_backoff.is_none(),
        "a refusal at stop sets no backoff"
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
    // The picker reads a third-party member's provider windows from the
    // third-party store, so the spent hybrid's reading lives there (a 5h bar at
    // 100%) rather than in the merged store.
    let spent = crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![crate::providers::UsageBar {
            label: "5h".to_string(),
            pct: 100.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
            used: None,
            total: None,
            inference_quota: false,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    let clear = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 10.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    daemon
        .third_party_usage_store
        .lock()
        .expect("third-party usage")
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
        &daemon.third_party_usage_store,
        &daemon.usage_status,
        &daemon.third_party_status,
        &daemon.third_party_streaks,
        &daemon.third_party_broken,
        &daemon.skipped_unreadable,
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
        &daemon.third_party_usage_store,
        &daemon.usage_status,
        &daemon.third_party_status,
        &daemon.third_party_streaks,
        &daemon.third_party_broken,
        &daemon.skipped_unreadable,
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

/// A probe switched off in another process while the daemon holds the lease:
/// its reload leaves the account as a restart shows it and keeps the cadence
/// stamp, so `status.json` no longer publishes the last probe's status as
/// `Fresh` and the session walk no longer reads the reading as fresh. An
/// account whose probe stays on keeps what its probe left.
#[test]
fn a_reload_that_finds_the_probe_switched_off_clears_what_it_left() {
    let _home = HomeSandbox::new();
    let names = [
        crate::profile::ProfileName::from("off"),
        crate::profile::ProfileName::from("on"),
    ];
    let profiles: Vec<Profile> = names
        .iter()
        .map(|name| {
            let mut p = blank_profile(name);
            p.usage_probe = true;
            // The probe reads only an account that stores a setup token.
            crate::testutil::write_long_lived_sidecar(name);
            p.has_long_lived_sidecar = true;
            p
        })
        .collect();
    let config = persist(profiles.clone(), Some("on"), 90_000);
    let mut daemon = daemon_for(config);
    for name in &names {
        crate::profile_cache::write_profile_cache(
            name,
            crate::profile_cache::USAGE_CACHE_FILE,
            &crate::usage::UsageInfo {
                fetched_at: Some(now_ms()),
                ..crate::usage::UsageInfo::default()
            },
        );
        daemon
            .next_refresh_per_profile
            .lock()
            .unwrap()
            .insert(FetchLeg::OAuth.key(name.clone()), 1);
        daemon
            .suppressed_auth_expired
            .lock()
            .unwrap()
            .insert(name.to_string(), 7);
    }
    crate::usage::bootstrap_fetch(
        &daemon.usage_store,
        &daemon.usage_status,
        &daemon.last_fetched,
        &[("off".to_string(), 90_000), ("on".to_string(), 90_000)],
    );

    let mut off = profiles[0].clone();
    off.usage_probe = false;
    save_profile(&off).expect("switch the probe off elsewhere");
    let config_toml =
        crate::profile::profile_subpath(&names[0], "config.toml").expect("config path");
    set_mtime(&config_toml, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();

    for (name, kept) in [(&names[0], false), (&names[1], true)] {
        let key = FetchLeg::OAuth.key(name.clone());
        let held = |present: bool, what: &str| assert_eq!(present, kept, "{name}: {what}");
        held(
            daemon
                .usage_store
                .lock()
                .unwrap()
                .contains_key(name.as_str()),
            "reading",
        );
        held(
            daemon
                .usage_status
                .lock()
                .unwrap()
                .contains_key(name.as_str()),
            "status",
        );
        held(
            daemon
                .next_refresh_per_profile
                .lock()
                .unwrap()
                .contains_key(&key),
            "countdown",
        );
        held(
            daemon
                .suppressed_auth_expired
                .lock()
                .unwrap()
                .contains_key(name.as_str()),
            "rest",
        );
        assert!(
            daemon.last_fetched.lock().unwrap().contains_key(&key),
            "{name}: the cadence stamp stays"
        );
    }
}

/// A setup token changed by hand in the same reload as the switch-off. clauth's
/// own writers drop the disk cache with the old token, a hand edit leaves it,
/// so the reload compare forgets the old token and seeds its last reading back
/// from disk, and the switch-off still clears it, since it runs after the
/// compare.
#[test]
fn a_switch_off_beside_a_token_changed_by_hand_leaves_no_re_seeded_reading() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("acct");
    let mut profile = blank_profile(&name);
    profile.usage_probe = true;
    profile.has_long_lived_sidecar = true;
    let config = persist(vec![profile.clone()], Some("acct"), 90_000);
    crate::claude::write_session_token(&name, "oat-first", 0).expect("write the sidecar");
    let mut daemon = daemon_for(config);
    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            fetched_at: Some(now_ms()),
            ..crate::usage::UsageInfo::default()
        },
    );
    crate::usage::bootstrap_fetch(
        &daemon.usage_store,
        &daemon.usage_status,
        &daemon.last_fetched,
        &[("acct".to_string(), 90_000)],
    );

    profile.usage_probe = false;
    save_profile(&profile).expect("switch the probe off elsewhere");
    let sidecar =
        crate::profile::profile_subpath(&name, "session-token.json").expect("sidecar path");
    let text = std::fs::read_to_string(&sidecar).expect("read the sidecar");
    std::fs::write(&sidecar, text.replace("oat-first", "oat-second"))
        .expect("change the token by hand");
    let config_toml = crate::profile::profile_subpath(&name, "config.toml").expect("config path");
    set_mtime(&config_toml, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();

    assert!(!daemon.usage_store.lock().unwrap().contains_key("acct"));
    assert!(!daemon.usage_status.lock().unwrap().contains_key("acct"));
}

/// S2: a stale entry produced by a FAILED load (a broken sibling config.toml
/// keeps the token list from rebuilding after a peer re-login) is NOT fetched
/// while it stays stale — two ticks send zero `/usage` requests — and once the
/// broken file is repaired and the reload rebuilds the token list, the very
/// next tick fetches it (no cadence delays the new login). Drives the real
/// `Daemon::reload_if_changed`; the fetch count is the endpoint server's
/// `/api/oauth/usage` requests. Port of the reviewer's
/// `probe_r4_a_failed_load_leaves_a_stale_entry_with_no_reload_coming`.
#[test]
fn a_stale_entry_is_not_fetched_until_the_token_list_is_rebuilt() {
    use std::time::{Duration, SystemTime};
    let _home = HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints(4, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (
                200,
                r#"{"five_hour":{"utilization":1.0,"resets_at":"2099-01-01T00:00:00+00:00"}}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&_home, &base);

    let a = crate::profile::ProfileName::from("a");
    let b = crate::profile::ProfileName::from("b");
    let config = persist(
        vec![profile_with_creds("a", "access-A"), blank_profile(&b)],
        Some("a"),
        90_000,
    );
    let mut daemon = daemon_for(config);

    // Another process re-logs `a`; `b`'s config.toml is torn so the next load
    // fails and the token list cannot rebuild. The failed load keeps the OLD
    // fingerprint (so the repaired file still triggers a reload) and logs the
    // failure once.
    save_profile(&profile_with_creds("a", "access-A2")).expect("peer re-login");
    let cred = crate::profile::profile_subpath(&a, "credentials.json").expect("cred path");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(60));
    let b_cfg = crate::profile::profile_subpath(&b, "config.toml").expect("cfg path");
    std::fs::write(&b_cfg, "not = [valid toml").expect("break b");
    set_mtime(&b_cfg, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();
    daemon.reload_if_changed();

    let state = crate::usage::SchedulerState::over_shared(
        daemon.config.clone(),
        daemon.usage_tokens.clone(),
        daemon.usage_store.clone(),
        daemon.usage_status.clone(),
        daemon.last_fetched.clone(),
        daemon.poll_streaks.clone(),
        daemon.snapshot.clone(),
    );
    // Two ticks over the stale entry: neither fetches it (it is dropped at the
    // partition, not spawned).
    crate::usage::tick(&state);
    crate::usage::tick(&state);

    // Repair `b`'s config: the next reload succeeds and rebuilds the token list
    // with `a`'s NEW credential, which matches the snapshot half.
    // A save refuses a file that is not TOML, so the repair removes it first.
    std::fs::remove_file(&b_cfg).expect("remove the broken file");
    save_profile(&blank_profile(&b)).expect("repair b");
    daemon.reload_if_changed();
    crate::usage::tick(&state);

    drop(_endpoints);
    let paths = server.join().expect("listener");
    assert_eq!(
        paths.iter().filter(|p| *p == "/api/oauth/usage").count(),
        1,
        "exactly the rebuilt entry's fetch lands — the two stale ticks sent nothing"
    );
    assert!(
        daemon.usage_store.lock().unwrap().contains_key("a"),
        "the rebuilt entry's fetch lands a reading"
    );
}

/// A copy-back writes another account's login into `credentials.json` and
/// touches nothing else under the profile. The daemon's next `/usage` read for
/// that profile must carry the stored token, never the displaced one it held
/// in memory: the displaced token stays valid for its own account, so a fetch
/// with it reports that account's usage under this profile's name.
#[test]
fn a_copy_back_moves_the_next_usage_bearer_to_the_stored_token() {
    use std::time::{Duration, SystemTime};
    let _home = HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints_raw(4, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (
                200,
                r#"{"five_hour":{"utilization":1.0,"resets_at":"2099-01-01T00:00:00+00:00"}}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&_home, &base);

    let a = crate::profile::ProfileName::from("a");
    let config = persist(vec![profile_with_creds("a", "access-A")], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    // The copy-back: the store alone is rewritten with another account's pair.
    let cred = crate::profile::profile_subpath(&a, "credentials.json").expect("cred path");
    std::fs::write(
        &cred,
        serde_json::to_vec(&oauth_creds("access-B")).expect("encode copied login"),
    )
    .expect("copy-back write");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(60));

    daemon.reload_if_changed();
    let state = crate::usage::SchedulerState::over_shared(
        daemon.config.clone(),
        daemon.usage_tokens.clone(),
        daemon.usage_store.clone(),
        daemon.usage_status.clone(),
        daemon.last_fetched.clone(),
        daemon.poll_streaks.clone(),
        daemon.snapshot.clone(),
    );
    crate::usage::tick(&state);

    drop(_endpoints);
    let requests = server.join().expect("listener");
    let bearers: Vec<String> = requests
        .iter()
        .filter(|raw| crate::testutil::request_path(raw).starts_with("/api/oauth/usage"))
        .filter_map(|raw| crate::testutil::request_header(raw, "authorization"))
        .collect();
    assert_eq!(
        bearers,
        vec!["Bearer access-B".to_string()],
        "the one /usage read after the copy-back carries the stored token"
    );
}

/// A copy-back that lands on an anchored account between the tick's reload and
/// a queued switch must still reach the token list. The switch saves its own
/// write while the other account's newer store is already on disk, and the
/// anchor still names the old account, so nothing may treat that store change
/// as this process's own.
#[test]
fn a_switch_never_swallows_another_accounts_copy_back() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
            profile_with_creds("gamma", "at-G1"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let gamma = crate::profile::ProfileName::from("gamma");
    crate::usage::seed_login_anchor(&gamma, Some(&crate::profile::AccountId::from("uuid-G")));
    let mut daemon = daemon_for(config);
    let cred = crate::profile::profile_subpath(&gamma, "credentials.json").expect("cred path");
    crate::profile::atomic_write_600(
        &cred,
        serde_json::to_vec(&oauth_creds("at-G2")).expect("encode copied login"),
    )
    .expect("copy-back write");

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "the switch landed"
    );
    daemon.reload_if_changed();

    let tokens: Vec<String> = daemon
        .usage_tokens
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.name == gamma)
        .map(|e| e.access_token.clone())
        .collect();
    assert_eq!(
        tokens,
        vec!["at-G2".to_string()],
        "gamma's entry follows its store"
    );
}

/// A rotation whose persist failed leaves its pair staged beside a store that
/// still holds the spent one. A token-list rebuild triggered by any external
/// change must take the staged pair, never the store's: the store's refresh
/// token is spent, and the next rotation would replay it.
#[test]
fn a_rebuild_keeps_a_rotated_pair_whose_persist_failed() {
    use std::time::{Duration, SystemTime};
    let _home = HomeSandbox::new();
    let c = crate::profile::ProfileName::from("c");
    let config = persist(vec![profile_with_creds("c", "at-old")], Some("c"), 90_000);
    let dir = crate::profile::profile_dir(&c).expect("profile dir");
    std::fs::write(
        dir.join("credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"at-old","refreshToken":"rt-old","expiresAt":4102444800000,"subscriptionType":"max"}}"#,
    )
    .expect("write the store");
    // base: the first 8 bytes of sha256("rt-old"), hex.
    std::fs::write(
        dir.join("credentials.json.staged"),
        r#"{"base":"068bfce5d78d07ac","base_access":"at-old",
            "creds":{"claudeAiOauth":{"accessToken":"at-mid","refreshToken":"rt-mid",
                     "expiresAt":4102444800000,"subscriptionType":"max"}}}"#,
    )
    .expect("write the stage");
    let mut daemon = daemon_for(config);

    let state_path = clauth_dir().unwrap().join("profiles.toml");
    set_mtime(&state_path, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();

    let pairs: Vec<(String, Option<String>)> = daemon
        .usage_tokens
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.name == c)
        .map(|e| (e.access_token.clone(), e.refresh_token.clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![("at-mid".to_string(), Some("rt-mid".to_string()))],
        "the rebuilt entry carries the staged pair"
    );
}

/// S3: a `forget_memory` whose identity read is unreadable (a torn identity
/// file) still keeps the snapshot from lagging its epoch — it marks the half
/// unknown, so the partition skips every entry of that name until the next
/// compare writes a real value. A tick over the unknown half fetches nothing;
/// a repaired file + reload clears the mark and the next tick fetches.
#[test]
fn a_forget_over_a_torn_identity_file_skips_the_entry_until_the_next_compare() {
    use std::time::{Duration, SystemTime};
    let _home = HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints(4, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (
                200,
                r#"{"five_hour":{"utilization":1.0,"resets_at":"2099-01-01T00:00:00+00:00"}}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&_home, &base);

    let a = crate::profile::ProfileName::from("a");
    let config = persist(vec![profile_with_creds("a", "access-A")], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    // Tear the identity file, then forget: the read is unreadable, so the half
    // is marked unknown (the snapshot must not keep a value its epoch outran).
    let cred = crate::profile::profile_subpath(&a, "credentials.json").expect("cred path");
    std::fs::write(&cred, "{ torn json").expect("tear credentials");
    crate::usage::forget_memory(&daemon.memory_stores(), &a, crate::usage::Half::OAuth);

    let state = crate::usage::SchedulerState::over_shared(
        daemon.config.clone(),
        daemon.usage_tokens.clone(),
        daemon.usage_store.clone(),
        daemon.usage_status.clone(),
        daemon.last_fetched.clone(),
        daemon.poll_streaks.clone(),
        daemon.snapshot.clone(),
    );
    // A tick over the unknown half fetches nothing: every entry of that name
    // is skipped while the half is unknown.
    crate::usage::tick(&state);

    // Repair the file and reload: the compare writes the real identity and
    // clears the unknown mark, so the next tick fetches.
    save_profile(&profile_with_creds("a", "access-A")).expect("repair credentials");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();
    crate::usage::tick(&state);

    drop(_endpoints);
    let paths = server.join().expect("listener");
    assert_eq!(
        paths.iter().filter(|p| *p == "/api/oauth/usage").count(),
        1,
        "only the post-repair fetch lands — the tick over the unknown half sent nothing"
    );
    assert!(
        daemon.usage_store.lock().unwrap().contains_key("a"),
        "the repaired entry's fetch lands a reading"
    );
}

/// A self-write site's drain (`apply_forgets`) forgets only this process's own
/// change, so an external re-login keeps the OLD fingerprint and the next
/// reload re-reads config and rebuilds the token list with the new credential.
#[test]
fn a_self_write_drain_keeps_the_old_fingerprint_for_the_next_reload() {
    use crate::profile_cache::USAGE_CACHE_FILE;
    use std::time::{Duration, SystemTime};

    let _home = HomeSandbox::new();
    let a = profile_with_creds("a", "access-A");
    let config = persist(vec![a.clone()], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    assert_eq!(daemon_token(&daemon, "a").as_deref(), Some("access-A"));

    // Another process re-logs `a` (new token) and drops the cache.
    let a_b = profile_with_creds("a", "access-B");
    save_profile(&a_b).expect("persist a's new login");
    crate::profile_cache::remove_profile_cache(
        &crate::profile::ProfileName::from("a"),
        USAGE_CACHE_FILE,
    );
    let cred = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        "credentials.json",
    )
    .expect("credentials path");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(60));

    // A self-write site's drain (the switch path's tail) forgets only this
    // process's own change and keeps the OLD fingerprint.
    daemon.apply_forgets();
    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("access-A"),
        "the token list still holds the old token until the reload"
    );

    daemon.reload_if_changed();
    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("access-B"),
        "the kept-old fingerprint makes the reload fire and rebuild the token list"
    );
}

/// A torn sibling `config.toml` fails the reload, but a failed load never
/// re-baselines the fingerprint: the failure logs once across several ticks
/// (deduped, not adopted), and once the file is repaired the reload re-reads
/// disk and follows a copy-back that landed while it was broken.
#[test]
fn a_torn_sibling_config_never_adopts_and_the_repaired_reload_follows_the_copy_back() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("a", "access-A"),
            profile_with_creds("b", "access-B"),
        ],
        Some("a"),
        90_000,
    );
    let mut daemon = daemon_for(config);

    let b_cfg =
        crate::profile::profile_subpath(&crate::profile::ProfileName::from("b"), "config.toml")
            .expect("config path");
    std::fs::write(&b_cfg, "not = [valid toml").expect("write broken config");
    set_mtime(&b_cfg, SystemTime::now() + Duration::from_secs(5));

    // A copy-back on `a` while the sibling config is torn.
    let a_cred = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        "credentials.json",
    )
    .expect("credentials path");
    crate::profile::atomic_write_600(
        &a_cred,
        serde_json::to_vec(&oauth_creds("access-A2")).expect("encode copied login"),
    )
    .expect("copy-back write");

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    daemon.reload_if_changed();
    daemon.reload_if_changed();
    daemon.reload_if_changed();

    let log = lines.snapshot();
    assert_eq!(
        log.iter()
            .filter(|l| l.contains("config reload failed"))
            .count(),
        1,
        "the failure logs once across several ticks, not per tick: {log:?}"
    );
    assert_ne!(
        daemon.last_reload_fp,
        reload_fingerprint(),
        "a failed load keeps the last-good fingerprint"
    );

    // Repair the sibling, then the reload must re-read the store.
    std::fs::write(&b_cfg, "").expect("repair the broken config");
    set_mtime(&b_cfg, SystemTime::now() + Duration::from_secs(10));

    daemon.reload_if_changed();

    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("access-A2"),
        "the repaired reload follows the copy-back"
    );
}

/// A repair that keeps the broken write's mtime (`touch -r` off the broken file,
/// a coarse clock) leaves the fingerprint where the failed load saw it, so only
/// a failed load that kept the last-good fingerprint retries it and follows a
/// copy-back that landed before the break was read.
#[test]
fn a_repair_keeping_the_broken_mtime_still_reloads_and_follows_the_copy_back() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("a", "access-A"),
            profile_with_creds("b", "access-B"),
        ],
        Some("a"),
        90_000,
    );
    let mut daemon = daemon_for(config);

    let a_cred = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        "credentials.json",
    )
    .expect("credentials path");
    crate::profile::atomic_write_600(
        &a_cred,
        serde_json::to_vec(&oauth_creds("access-A2")).expect("encode copied login"),
    )
    .expect("copy-back write");
    let b_cfg =
        crate::profile::profile_subpath(&crate::profile::ProfileName::from("b"), "config.toml")
            .expect("config path");
    let broken_at = SystemTime::now() + Duration::from_secs(5);
    std::fs::write(&b_cfg, "not = [valid toml").expect("write broken config");
    set_mtime(&b_cfg, broken_at);

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    daemon.reload_if_changed();
    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("access-A"),
        "the failed load rebuilt nothing"
    );

    std::fs::write(&b_cfg, "").expect("repair the broken config");
    set_mtime(&b_cfg, broken_at);
    daemon.reload_if_changed();

    let log = lines.snapshot();
    assert_eq!(reload_count(&log), 1, "the repair reloads once: {log:?}");
    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("access-A2"),
        "the repaired reload follows the copy-back"
    );
}

// ── cross-process RMW atomicity ───────────────────────────────────────────────

/// After a switch, the daemon's `last_reload_fp` is NOT the on-disk fingerprint:
/// the switch write is left unadopted, so the next reload re-reads, adopts and
/// logs it once. A later external write is reloaded and logs once more. The
/// no-adoption rule is pinned at the daemon and TUI switch sites, the theme
/// toggle and the log out; the daemon's switch-off and quarantine writes and the
/// TUI's other own writes are not.
#[test]
fn rmw_switch_leaves_own_write_unadopted_then_reloads_external() {
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

    assert_ne!(
        daemon.last_reload_fp,
        reload_fingerprint(),
        "the daemon leaves its own switch write unadopted"
    );

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    daemon.reload_if_changed();
    let log = lines.snapshot();
    assert_eq!(
        daemon.last_reload_fp,
        reload_fingerprint(),
        "the reload adopts the on-disk fingerprint it just re-read"
    );
    assert_eq!(
        daemon.refresh_interval.load(Ordering::Relaxed),
        90_000,
        "the reload of the daemon's own switch leaves the interval unchanged"
    );
    assert_eq!(
        reload_count(&log),
        1,
        "a reload that loaded logs once, own write or not: {log:?}"
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
    let external_log = lines.snapshot();
    assert_eq!(
        reload_count(&external_log),
        2,
        "the later external reload logs once more: {external_log:?}"
    );
}

// ── reload compare ───────────────────────────────────────────────────────────

/// The daemon's reload forgets a half another process changed — an OAuth
/// token change clears the OAuth reading, a third-party key change clears only
/// the provider reading (a hybrid's OAuth leg stays).
#[test]
fn a_daemon_reload_forgets_external_oauth_and_third_party_changes() {
    use crate::profile::{Profile, save_profile};
    use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE};
    use crate::usage::FetchStatus;
    use std::time::{Duration, SystemTime};

    let _home = HomeSandbox::new();

    let a = profile_with_creds("a", "access-A");
    let mut hybrid = Profile::new(
        "hybrid".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("old-key".to_string()),
    );
    hybrid.credentials = Some(oauth_creds("hybrid-A"));

    let config = persist(vec![a.clone(), hybrid.clone()], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    // Seed the OAuth readings through the real boot seed (which also stamps
    // `last_fetched`), and plant the provider reading by hand.
    let info = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        ..Default::default()
    };
    let stats = crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 80.0)]);
    for name in ["a", "hybrid"] {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(name),
            USAGE_CACHE_FILE,
            &info,
        );
    }
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("hybrid"),
        THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    crate::usage::bootstrap_fetch(
        &daemon.usage_store,
        &daemon.usage_status,
        &daemon.last_fetched,
        &[("a".to_string(), 90_000), ("hybrid".to_string(), 90_000)],
    );
    daemon
        .third_party_usage_store
        .lock()
        .unwrap()
        .insert("hybrid".to_string(), stats.clone());
    daemon
        .third_party_status
        .lock()
        .unwrap()
        .insert("hybrid".to_string(), FetchStatus::Fresh);

    // Another process: `a` re-logs (new token), `hybrid` re-keys (same token).
    let a_b = profile_with_creds("a", "access-B");
    let mut hybrid_new = hybrid.clone();
    hybrid_new.api_key = Some("new-key".to_string());
    save_profile(&a_b).expect("persist a's new login");
    save_profile(&hybrid_new).expect("persist hybrid's new key");
    // The other process's forget_disk drops the caches the changed halves own.
    crate::profile_cache::remove_profile_cache(
        &crate::profile::ProfileName::from("a"),
        USAGE_CACHE_FILE,
    );
    crate::profile_cache::remove_profile_cache(
        &crate::profile::ProfileName::from("hybrid"),
        THIRD_PARTY_CACHE_FILE,
    );
    for (name, file) in [("a", "credentials.json"), ("hybrid", "config.toml")] {
        let path = crate::profile::profile_subpath(&crate::profile::ProfileName::from(name), file)
            .expect("changed file path");
        set_mtime(&path, SystemTime::now() + Duration::from_secs(60));
    }

    let hybrid_oauth_base = crate::usage::epoch_value(
        &crate::profile::ProfileName::from("hybrid"),
        crate::usage::Half::OAuth,
    );

    daemon.reload_if_changed();

    assert!(
        !daemon.usage_store.lock().unwrap().contains_key("a"),
        "a's OAuth reading goes with the external re-login"
    );
    assert!(
        !daemon
            .third_party_usage_store
            .lock()
            .unwrap()
            .contains_key("hybrid"),
        "the hybrid's provider reading goes with the external re-key"
    );
    assert!(
        daemon.usage_store.lock().unwrap().contains_key("hybrid"),
        "the hybrid's OAuth reading stays when only the key changed"
    );
    assert!(
        !daemon
            .third_party_status
            .lock()
            .unwrap()
            .contains_key("hybrid"),
        "the hybrid's provider status goes with the external re-key"
    );
    assert_eq!(
        crate::usage::epoch_value(
            &crate::profile::ProfileName::from("hybrid"),
            crate::usage::Half::OAuth
        ),
        hybrid_oauth_base,
        "only the provider half changed: the hybrid's OAuth epoch does not move"
    );
}

/// A no-op re-save written by another process forgets nothing.
#[test]
fn a_daemon_reload_forgets_nothing_on_a_noop_resave() {
    use crate::profile::save_profile;
    use crate::usage::{FetchStatus, Half, epoch_value};
    use std::time::{Duration, SystemTime};

    let _home = HomeSandbox::new();

    let a = profile_with_creds("a", "access-A");
    let config = persist(vec![a.clone()], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    daemon.usage_store.lock().unwrap().insert(
        "a".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.0,
                resets_at: None,
            }),
            ..Default::default()
        },
    );
    daemon
        .usage_status
        .lock()
        .unwrap()
        .insert("a".to_string(), FetchStatus::Fresh);
    let base = epoch_value(&crate::profile::ProfileName::from("a"), Half::OAuth);

    save_profile(&a).expect("no-op re-save");
    let cred = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        "credentials.json",
    )
    .expect("credentials path");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(60));

    daemon.reload_if_changed();

    assert!(
        daemon.usage_store.lock().unwrap().contains_key("a"),
        "a no-op re-save forgets nothing"
    );
    assert_eq!(
        epoch_value(&crate::profile::ProfileName::from("a"), Half::OAuth),
        base,
        "the epoch does not move on a no-op re-save"
    );

    // An anchor backfill: the anchor appears over the unchanged stored token.
    crate::usage::seed_login_anchor(
        &crate::profile::ProfileName::from("a"),
        Some(&crate::profile::AccountId::from("uuid-A")),
    );
    let anchor = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
    )
    .expect("anchor path");
    set_mtime(&anchor, SystemTime::now() + Duration::from_secs(120));
    daemon.reload_if_changed();
    assert!(
        daemon.usage_store.lock().unwrap().contains_key("a"),
        "an anchor backfill over the same token forgets nothing"
    );
    assert_eq!(
        epoch_value(&crate::profile::ProfileName::from("a"), Half::OAuth),
        base,
        "the epoch does not move on an anchor backfill"
    );
}

/// N1: the reload writes the rebuilt token list BEFORE the compare's forget
/// (the epoch bump). A seed-hook park during the re-seed — which runs after the
/// bump inside `reload_and_forget` — observes the token list already holding the
/// new entry. On the old order (bump before rebuild) the park would still see
/// the old token, and a tick partitioning there would clone the old entry under
/// the post-bump epoch.
#[test]
fn a_reload_rebuilds_the_token_list_before_it_bumps_the_epoch() {
    use crate::profile::save_profile;
    use crate::profile_cache::USAGE_CACHE_FILE;
    use crate::usage::Half;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime};

    let _home = HomeSandbox::new();
    let a = profile_with_creds("a", "access-A");
    let config = persist(vec![a.clone()], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    // A cache present for `a` so the reload's re-seed reads it and fires the
    // seed hook (a removed cache would short-circuit the re-seed before the
    // hook).
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("a"),
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );

    // Another process re-logs `a`.
    let a_b = profile_with_creds("a", "access-B");
    save_profile(&a_b).expect("persist a's new login");
    let cred = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        "credentials.json",
    )
    .expect("credentials path");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(60));

    let tokens = daemon.usage_tokens.clone();
    // Baseline the epoch BEFORE the reload: the map is process-global and never
    // reset, so a prior test in the same process may already have forgotten `a`.
    let epoch_before =
        crate::usage::epoch_value(&crate::profile::ProfileName::from("a"), Half::OAuth);
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    crate::usage::set_seed_hook(
        crate::usage::SeedPhase::PostCompare,
        crate::usage::Half::OAuth,
        move |_name| {
            let _ = parked_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        },
    );

    struct Release(mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    std::thread::scope(|scope| {
        // Releases the parked re-seed on ANY exit (success or a red), so the
        // scope's join never hangs.
        let _release = Release(release_tx);
        scope.spawn(|| daemon.reload_if_changed());
        parked_rx.recv().expect("the reload parks in the re-seed");
        let token = tokens
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.name.as_str() == "a")
            .map(|t| t.access_token.clone());
        assert_eq!(
            token.as_deref(),
            Some("access-B"),
            "the token list is rebuilt before the epoch bump (N1)"
        );
    });

    assert_eq!(
        crate::usage::epoch_value(&crate::profile::ProfileName::from("a"), Half::OAuth),
        epoch_before + 1,
        "the external re-login still bumps the epoch exactly once"
    );
}

/// With another process paused mid-hold (new identity written, the old
/// cache already removed), the reload's re-seed brings back no old reading.
#[test]
fn a_daemon_reload_reseed_brings_back_no_old_reading() {
    use crate::profile::save_profile;
    use crate::profile_cache::USAGE_CACHE_FILE;
    use crate::usage::FetchLeg;
    use std::time::{Duration, SystemTime};

    let _home = HomeSandbox::new();

    let a = profile_with_creds("a", "access-A");
    let config = persist(vec![a.clone()], Some("a"), 90_000);
    let mut daemon = daemon_for(config);

    // Seed the reading from a real cache, the way the daemon's boot seed would.
    let info = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        ..Default::default()
    };
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("a"),
        USAGE_CACHE_FILE,
        &info,
    );
    crate::usage::bootstrap_fetch(
        &daemon.usage_store,
        &daemon.usage_status,
        &daemon.last_fetched,
        &[("a".to_string(), 90_000)],
    );
    assert!(daemon.usage_store.lock().unwrap().contains_key("a"));

    // Another process mid-hold: the identity write landed, forget_disk already
    // removed the old cache.
    let a_b = profile_with_creds("a", "access-B");
    save_profile(&a_b).expect("persist the new identity");
    crate::profile_cache::remove_profile_cache(
        &crate::profile::ProfileName::from("a"),
        USAGE_CACHE_FILE,
    );
    let cred = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("a"),
        "credentials.json",
    )
    .expect("credentials path");
    set_mtime(&cred, SystemTime::now() + Duration::from_secs(60));

    daemon.reload_if_changed();

    assert!(
        !daemon.usage_store.lock().unwrap().contains_key("a"),
        "the reload's re-seed brings back no old reading once the cache is gone"
    );
    assert!(
        !daemon
            .last_fetched
            .lock()
            .unwrap()
            .contains_key(&FetchLeg::OAuth.key(crate::profile::ProfileName::from("a"))),
        "the reload drops the old cadence stamp too"
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

    let lines = crate::logline::LogLines::new();
    let guard = lines.capture_here();
    daemon.drain_pending_switch();
    drop(guard);

    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: gave up switching to 'beta' (last deferral: target is mid-fetch)"
                .to_string()
        ],
        "a readable target's give-up names its last deferral as past, and no skip"
    );
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

/// A store that deferred unreadable and reads again by the time the window
/// closes is no longer the cause: the give-up names the unreadable read as the
/// last deferral, never as the present state, and skips nothing.
#[test]
fn a_give_up_on_a_store_that_reads_again_names_the_unreadable_read_as_past() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("delta", "at-delta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let creds = crate::profile::profile_dir(&crate::profile::ProfileName::from("delta"))
        .expect("profile dir")
        .join("credentials.json");
    let readable = std::fs::read(&creds).expect("read stored credentials");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "delta");
    let lines = crate::logline::LogLines::new();
    let guard = lines.capture_here();
    std::fs::write(&creds, "{\"claudeAiOauth\": {\"accessToken\": ").expect("tear the store");
    daemon.drain_pending_switch();

    std::fs::write(&creds, &readable).expect("restore the store");
    let now = now_ms();
    let backoff = daemon
        .switch_backoff
        .as_mut()
        .expect("the unreadable drain defers through the backoff");
    backoff.not_before = now.saturating_sub(1);
    backoff.retry_until = now.saturating_sub(1);
    daemon.drain_pending_switch();
    drop(guard);

    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: deferring switch to 'delta': its stored login could not be read"
                .to_string(),
            "clauth daemon: gave up switching to 'delta' \
             (last deferral: its stored login could not be read)"
                .to_string(),
        ],
        "the give-up states the unreadable read as the last deferral only"
    );
    assert!(
        !daemon
            .skipped_unreadable
            .lock()
            .expect("skipped set")
            .contains("delta"),
        "a store that reads again is not skipped"
    );
}

// ── uncapped_spenders (boot-time warning's pure collection) ───────────────────

/// A disabled member is never spend-armed by the walk, so it must never be
/// named in the "can spend with no cap" warning — only a live, enabled
/// uncapped sibling should surface.
#[test]
fn uncapped_spenders_excludes_disabled_includes_enabled_sibling() {
    let _home = HomeSandbox::new();
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

/// A last-resort sink whose `usage_cache.json` plan is canceled catches no
/// spend: the boot warning reads the canceled verdict off disk (the daemon has
/// no live store yet at boot), so the capped spender is named.
#[test]
fn uncapped_spenders_names_a_spender_whose_only_sink_is_canceled_on_disk() {
    use crate::usage::{PlanInfo, UsageInfo};
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["spender", "sink"]);
    let mut spender = blank_profile(&crate::profile::ProfileName::from("spender"));
    spender.max_auto_spend = Some(5.0);
    let mut sink = blank_profile(&crate::profile::ProfileName::from("sink"));
    sink.last_resort = true;
    crate::profile_cache::write_profile_cache(
        &sink.name,
        crate::profile_cache::USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                subscription_status: Some("canceled".to_string()),
                ..PlanInfo::default()
            }),
            ..UsageInfo::default()
        },
    );
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["spender".into(), "sink".into()],
            spend_budget_switching: true,
            switch_off_when_budget_spent: false,
            ..AppState::default()
        },
        profiles: vec![spender, sink],
    };

    assert_eq!(
        super::uncapped_spenders(&config),
        vec!["spender"],
        "a canceled last-resort sink stops nothing, so the spender is uncapped"
    );
}

/// An organization's refusal on the sink's cached plan is the same dead state:
/// the sink catches no spend, so the spender is named. The control: the same
/// sink with no refusal on its plan stops the spender.
#[test]
fn uncapped_spenders_names_a_spender_whose_only_sink_its_organization_refuses_on_disk() {
    use crate::usage::{PlanInfo, UsageInfo};
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["spender", "sink"]);
    let mut spender = blank_profile(&crate::profile::ProfileName::from("spender"));
    spender.max_auto_spend = Some(5.0);
    let mut sink = blank_profile(&crate::profile::ProfileName::from("sink"));
    sink.last_resort = true;
    // A sink with a login: one with none is no parking spot either way.
    sink.credentials = Some(crate::testutil::oauth_pair("sink"));
    let sink_name = sink.name.clone();
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["spender".into(), "sink".into()],
            spend_budget_switching: true,
            switch_off_when_budget_spent: false,
            ..AppState::default()
        },
        profiles: vec![spender, sink],
    };
    let write_plan = |status: Option<&str>| {
        crate::profile_cache::write_profile_cache(
            &sink_name,
            crate::profile_cache::USAGE_CACHE_FILE,
            &UsageInfo {
                plan: Some(PlanInfo {
                    subscription_status: status.map(str::to_string),
                    ..PlanInfo::default()
                }),
                ..UsageInfo::default()
            },
        );
    };
    write_plan(None);
    assert!(
        super::uncapped_spenders(&config).is_empty(),
        "control: a serving last-resort sink stops the spender"
    );
    write_plan(Some(crate::usage::ORG_REFUSED_STATUS));
    assert_eq!(super::uncapped_spenders(&config), vec!["spender"]);
}

/// A hybrid last-resort sink serves through its provider key, which a canceled
/// OAuth plan does not stop: its picked reading is the provider's (no plan), so
/// the boot warning counts it as a sink, as the TUI's member card does, and
/// names no spender.
#[test]
fn uncapped_spenders_counts_a_hybrid_sink_whose_oauth_plan_is_canceled() {
    use crate::usage::{PlanInfo, UsageInfo};
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["spender", "sink"]);
    let mut spender = blank_profile(&crate::profile::ProfileName::from("spender"));
    spender.max_auto_spend = Some(5.0);
    let mut sink = profile_with_creds("sink", "a-access");
    sink.base_url = Some("https://api.deepseek.com/anthropic".into());
    sink.api_key = Some("k".into());
    sink.provider = Some(crate::providers::Provider::DeepSeek);
    sink.last_resort = true;
    assert!(
        sink.usage_cache_is_third_party(),
        "fixture: the hybrid's figures live in its provider cache"
    );
    crate::profile_cache::write_profile_cache(
        &sink.name,
        crate::profile_cache::USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                subscription_status: Some("canceled".to_string()),
                ..PlanInfo::default()
            }),
            ..UsageInfo::default()
        },
    );
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["spender".into(), "sink".into()],
            spend_budget_switching: true,
            switch_off_when_budget_spent: false,
            ..AppState::default()
        },
        profiles: vec![spender, sink],
    };

    assert_eq!(
        super::uncapped_spenders(&config),
        Vec::<&str>::new(),
        "the hybrid sink still catches the spend, so nothing is uncapped"
    );
}

/// `fetch_status` of each profile in a `status.json` body built the way
/// `write_status` builds it, off the daemon's own live stores.
fn published_fetch_status(daemon: &Daemon) -> Vec<(String, serde_json::Value)> {
    let snap = daemon.live_stores().snapshot();
    let live = snap.signals();
    let cfg = daemon.config.lock().unwrap().clone();
    let interval = daemon.refresh_interval.load(Ordering::Relaxed);
    let body = super::status_json::build_status(&cfg, interval, Some(&live), true);
    let v = serde_json::to_value(&body).unwrap();
    v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap().to_string(),
                p["fetch_status"].clone(),
            )
        })
        .collect()
}

/// A disabled provider account gets no boot seed at all (the daemon's boot seed
/// is the work list, and nothing polls a disabled account), so `status.json`
/// keeps the daemonless arms for it — a durable auth-expired verdict for its key
/// publishes `AuthExpired`, and a keyless one's just-written cache derives
/// `Fresh`.
#[test]
fn the_boot_seed_leaves_a_disabled_accounts_published_status_to_its_durable_verdict() {
    use crate::profile::ProfileName;
    let _home = HomeSandbox::new();
    let provider = |n: &str, keyed: bool| {
        let mut p = blank_profile(&ProfileName::from(n));
        p.base_url = Some("https://api.deepseek.com/anthropic".into());
        if keyed {
            p.api_key = Some("k".into());
        }
        p.provider = Some(crate::providers::Provider::DeepSeek);
        p.disabled = true;
        p
    };
    // The work-list positive: an ENABLED keyed account with the same durable
    // auth-expired record and fresh cache publishes the boot-seeded `Fresh`
    // (nothing polls it into a live verdict; the durable one must lose).
    let mut on = blank_profile(&ProfileName::from("on"));
    on.base_url = Some("https://api.deepseek.com/anthropic".into());
    on.api_key = Some("k".into());
    on.provider = Some(crate::providers::Provider::DeepSeek);
    let profiles = vec![provider("dx", true), provider("kl", false), on.clone()];
    let fp = crate::usage::profile_credential_fingerprint(&profiles[0]).expect("fingerprint");
    let on_fp = crate::usage::profile_credential_fingerprint(&on).expect("on fingerprint");
    let config = persist(profiles, None, 90_000);
    for n in ["dx", "kl", "on"] {
        crate::profile_cache::write_profile_cache(
            &ProfileName::from(n),
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            &crate::testutil::stats_with_bars(vec![
                crate::testutil::bar("5h", 47.0),
                crate::testutil::bar("7d", 23.0),
            ]),
        );
    }
    crate::profile_cache::write_auth_expired(&ProfileName::from("dx"), fp);
    crate::profile_cache::write_auth_expired(&ProfileName::from("on"), on_fp);
    let daemon = daemon_for(config);
    daemon.boot();
    daemon.shutting_down.store(true, Ordering::SeqCst);

    assert_eq!(
        published_fetch_status(&daemon),
        vec![
            ("dx".to_string(), serde_json::json!("AuthExpired")),
            ("kl".to_string(), serde_json::json!("Fresh")),
            ("on".to_string(), serde_json::json!("Fresh")),
        ],
        "a disabled account publishes its durable verdict, never a boot-seeded live status; an enabled one gets the boot-seeded Fresh over the same durable record"
    );
}

/// The derived arm ages: a disabled account's cache, fresh at boot, publishes
/// `Cached` once it is older than one interval, because no boot-seeded `Fresh`
/// status outranks the derivation for a name nothing polls.
#[test]
fn a_disabled_accounts_published_status_ages_to_cached_after_boot() {
    use crate::profile::ProfileName;
    let _home = HomeSandbox::new();
    let mut p = blank_profile(&ProfileName::from("dx"));
    p.base_url = Some("https://api.deepseek.com/anthropic".into());
    p.api_key = Some("k".into());
    p.provider = Some(crate::providers::Provider::DeepSeek);
    p.disabled = true;
    let config = persist(vec![p], None, 90_000);
    crate::profile_cache::write_profile_cache(
        &ProfileName::from("dx"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 47.0)]),
    );
    let daemon = daemon_for(config);
    daemon.boot();
    daemon.shutting_down.store(true, Ordering::SeqCst);
    let path = crate::profile::profile_dir(&ProfileName::from("dx"))
        .unwrap()
        .join(crate::profile_cache::THIRD_PARTY_CACHE_FILE);
    set_mtime(&path, SystemTime::now() - Duration::from_secs(2 * 3600));

    assert_eq!(
        published_fetch_status(&daemon),
        vec![("dx".to_string(), serde_json::json!("Cached"))],
        "a two-hour-old cache publishes Cached, not the boot's Fresh"
    );
}

/// The daemon has no display reader of its provider store (`status.json` reads
/// provider figures off disk), so the boot seed is the work list, not the
/// display superset: a keyless provider chain member's cache never enters the
/// store, and the daemon scan picks exactly what 6836c1a6 picked — the keyless
/// member, whose absent reading reads as headroom.
#[test]
fn the_daemon_scan_ignores_a_keyless_provider_members_cached_reading() {
    for (label, chain, ok_age) in [
        ("S1 chain[act,kl]", vec!["act", "kl"], 0u64),
        (
            "S3 chain[act,kl,ok] ok-aged",
            vec!["act", "kl", "ok"],
            7200u64,
        ),
    ] {
        let _home = HomeSandbox::new();
        crate::testutil::keyless_walk_fixture(&chain, "act", ok_age, 97.0);
        let d = daemon_for(crate::profile::load_config().expect("load config"));
        d.boot();
        d.shutting_down.store(true, Ordering::SeqCst);
        assert!(
            d.third_party_usage_store
                .lock()
                .unwrap()
                .get("kl")
                .is_none(),
            "{label}: the boot seed is the work list, never the display superset"
        );
        crate::usage::scan_auto_switch(
            &d.config,
            &d.usage_store,
            &d.third_party_usage_store,
            &d.usage_status,
            &d.third_party_status,
            &d.third_party_streaks,
            &d.third_party_broken,
            &d.skipped_unreadable,
            &d.poll_streaks,
            &d.kick_blocks,
            &d.activity,
            &d.pending_switch,
            &d.pending_switch_off,
        );
        assert_eq!(
            queued_targets(&d),
            vec!["kl".to_string()],
            "{label}: the daemon scan"
        );
    }
}

// ── no_usage_source_members (boot-time warning's pure collection, #110) ───────

/// Install a long-lived mint sidecar for `name`: the half that makes a
/// credential-less member one clauth cannot MEASURE rather than one that is
/// simply logged out. The daemon has no per-frame cache to read it from, so it
/// reads the file itself, once, at boot. The dir is created the way
/// `save_profile` would, since these fixtures never persist their profiles.
fn arm_session_token(name: &str) {
    let name = crate::profile::ProfileName::from(name);
    std::fs::create_dir_all(crate::profile::profile_dir(&name).expect("profile dir"))
        .expect("mkdir profile");
    crate::claude::write_session_token(&name, "oat-mint", 0).expect("write the sidecar");
}

/// Only an enabled member whose sessions run on a long-lived token with no
/// OAuth pair is named: that is the one shape no leg ever reads while something
/// still serves from it. A pair, an api key, a disabled flag (the walk never
/// switches to it), or no login at all keeps a member out.
#[test]
fn no_usage_source_members_names_only_enabled_members_clauth_cannot_measure() {
    let _home = HomeSandbox::new();
    let name = |n: &str| crate::profile::ProfileName::from(n);
    let mut keyed = blank_profile(&name("keyed"));
    keyed.api_key = Some("sk-test".into());
    let mut off = blank_profile(&name("off"));
    off.disabled = true;
    // Every member below is credential-less; the sidecar is what sorts them.
    for armed in ["st-a", "keyed", "off", "st-b"] {
        arm_session_token(armed);
    }
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec![
                "polled".into(),
                "st-a".into(),
                "keyed".into(),
                "off".into(),
                "st-b".into(),
                "out".into(),
            ],
            ..AppState::default()
        },
        profiles: vec![
            profile_with_creds("polled", "at-polled"),
            blank_profile(&name("st-a")),
            keyed,
            off,
            blank_profile(&name("st-b")),
            blank_profile(&name("out")),
        ],
    };
    assert_eq!(
        super::no_usage_source_members(&config),
        ["st-a", "st-b"],
        "`out` holds no login at all: nothing serves from it, so it is the \
         logged-out shape rather than this one"
    );
}

/// The warning reaches the log as one line naming every such member, with the
/// fix spelled out for each of them rather than left as a `<name>` placeholder
/// beside a list that just named them. Says nothing when the chain has none.
#[test]
fn warn_if_chain_has_no_usage_source_logs_one_line_naming_them() {
    let _home = HomeSandbox::new();
    let name = |n: &str| crate::profile::ProfileName::from(n);
    arm_session_token("st-a");
    arm_session_token("st-b");
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

    let logged = |chain: Vec<&str>| -> Vec<String> {
        let lines = crate::logline::LogLines::new();
        {
            let _capture = lines.capture_here();
            super::warn_if_chain_has_no_usage_source(&config(chain));
        }
        lines.snapshot()
    };

    assert_eq!(
        logged(vec!["polled", "st-a", "st-b"]),
        [
            "clauth daemon: can't read the usage of st-a, st-b. once one is active, clauth \
             won't switch away from it on its own. log in the same accounts with `clauth \
             login st-a` and `clauth login st-b`, or turn on their usage probes, so clauth \
             can read their usage"
        ],
    );
    // One member reads as one account, which is the wording the member card
    // carries too: the same fn, told a one-name list. The lead comes from the
    // card's own `no_usage_hazard_lead` for a one-member line as well, so the
    // whole sentence matches the card and the confirm.
    assert_eq!(
        logged(vec!["polled", "st-a"]),
        [
            "clauth daemon: can't read the usage of st-a. once it's active, clauth won't \
             switch away from it on its own. log in the same account with `clauth login \
             st-a`, or turn on its usage probe, so clauth can read its usage"
        ],
    );
    assert!(
        logged(vec!["polled"]).is_empty(),
        "a chain clauth can measure in full logs nothing"
    );
}

/// A member the probe reads is measured, so the boot warning leaves it out,
/// the member list and the logged line alike.
#[test]
fn warn_if_chain_has_no_usage_source_leaves_out_a_member_the_probe_reads() {
    let _home = HomeSandbox::new();
    let name = |n: &str| crate::profile::ProfileName::from(n);
    arm_session_token("st-a");
    let mut probed = blank_profile(&name("st-a"));
    probed.usage_probe = true;
    probed.has_long_lived_sidecar = true;
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["polled".into(), "st-a".into()],
            ..AppState::default()
        },
        profiles: vec![profile_with_creds("polled", "at-polled"), probed],
    };
    assert!(super::no_usage_source_members(&config).is_empty());
    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        super::warn_if_chain_has_no_usage_source(&config);
    }
    assert!(lines.snapshot().is_empty(), "{:?}", lines.snapshot());
}

/// A daemon can boot with such a member ALREADY active: the chain walked onto
/// it before this process started, or the operator put it there. Then the line
/// says so, because the hazard is the state of the machine and not a forecast.
#[test]
fn warn_if_chain_has_no_usage_source_names_an_active_member_in_the_present() {
    let _home = HomeSandbox::new();
    let name = |n: &str| crate::profile::ProfileName::from(n);
    arm_session_token("st-a");
    let config = |active: Option<&str>| AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            fallback_chain: vec!["polled".into(), "st-a".into()],
            ..AppState::default()
        },
        profiles: vec![
            profile_with_creds("polled", "at-polled"),
            blank_profile(&name("st-a")),
        ],
    };
    let logged = |active: Option<&str>| -> Vec<String> {
        let lines = crate::logline::LogLines::new();
        {
            let _capture = lines.capture_here();
            super::warn_if_chain_has_no_usage_source(&config(active));
        }
        lines.snapshot()
    };

    assert_eq!(
        logged(Some("st-a")),
        [
            "clauth daemon: can't read the usage of st-a. it's active, and clauth won't \
             switch away from it on its own. log in the same account with `clauth login \
             st-a`, or turn on its usage probe, so clauth can read its usage"
        ],
    );
    // The measurable member being active leaves the forecast wording, and so
    // does an empty active slot.
    for active in [Some("polled"), None] {
        assert_eq!(
            logged(active),
            [
                "clauth daemon: can't read the usage of st-a. once it's active, clauth won't \
                 switch away from it on its own. log in the same account with `clauth login \
                 st-a`, or turn on its usage probe, so clauth can read its usage"
            ],
            "not yet active: {active:?}"
        );
    }
}

/// The other direction of the lead: a LIST keeps this line's own, in both
/// tenses. `no_usage_hazard_lead` says `it`, which cannot name which of several
/// members, and the present-tense arm has to say which one is active — so the
/// shared lead is for the one-member line only and must not spread to a list.
#[test]
fn warn_if_chain_has_no_usage_source_keeps_its_own_lead_for_a_list() {
    let _home = HomeSandbox::new();
    let name = |n: &str| crate::profile::ProfileName::from(n);
    arm_session_token("st-a");
    arm_session_token("st-b");
    let config = |active: Option<&str>| AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            fallback_chain: vec!["polled".into(), "st-a".into(), "st-b".into()],
            ..AppState::default()
        },
        profiles: vec![
            profile_with_creds("polled", "at-polled"),
            blank_profile(&name("st-a")),
            blank_profile(&name("st-b")),
        ],
    };
    let logged = |active: Option<&str>| -> Vec<String> {
        let lines = crate::logline::LogLines::new();
        {
            let _capture = lines.capture_here();
            super::warn_if_chain_has_no_usage_source(&config(active));
        }
        lines.snapshot()
    };

    assert_eq!(
        logged(Some("st-b")),
        [
            "clauth daemon: can't read the usage of st-a, st-b. 'st-b' is active now, and clauth \
             won't switch away from it on its own. log in the same accounts with `clauth login \
             st-a` and `clauth login st-b`, or turn on their usage probes, so clauth can read \
             their usage"
        ],
        "a list names WHICH member is active"
    );
    for active in [Some("polled"), None] {
        assert_eq!(
            logged(active),
            [
                "clauth daemon: can't read the usage of st-a, st-b. once one is active, clauth \
                 won't switch away from it on its own. log in the same accounts with `clauth \
                 login st-a` and `clauth login st-b`, or turn on their usage probes, so clauth \
                 can read their usage"
            ],
            "a list that is not active keeps the list forecast: {active:?}"
        );
    }
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
    a.credentials = Some(crate::testutil::oauth_pair("work"));
    let mut b = blank_profile(&crate::profile::ProfileName::from("personal"));
    b.preferred_days = all();
    b.credentials = Some(crate::testutil::oauth_pair("personal"));

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

/// A credit refusal on record is said on the log at its edges, whichever
/// process wrote it: once when it lands, nothing while it stands, once when it
/// clears, and once more from a fresh daemon over a refusal still standing
/// (the in-memory latch, like the cap's). Driven by `write_status`, the
/// per-tick work both daemon modes run.
#[test]
fn the_daemon_says_a_credit_refusal_once_per_edge() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = persist(vec![blank_profile(&name)], Some("work"), 60_000);
    let mut daemon = daemon_for(config.clone());
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let refused = "work: credits refused; the global walk takes it after its headroom and home \
                   passes, and a session that may need credits after every other pass, until a \
                   request on it succeeds or a window reset leaves it headroom";
    let cleared = "work: credit refusal cleared; auto-switch takes it again for the global \
                   account and for any session";
    let refuse = |at_ms| {
        crate::profile_cache::write_credit_refusal(
            &name,
            &crate::profile_cache::CreditRefusal {
                at_ms,
                file: std::path::PathBuf::from("/home/u/.claude/projects/p/conv.jsonl"),
                uuid: None,
            },
        );
    };
    let now = crate::usage::now_ms();

    daemon.write_status();
    assert!(lines.snapshot().is_empty(), "no refusal, no line");
    refuse(now);
    daemon.write_status();
    assert_eq!(lines.snapshot(), [refused], "the refusal reaches the log");
    daemon.write_status();
    assert_eq!(
        lines.snapshot(),
        [refused],
        "an unchanged tick writes nothing"
    );
    crate::profile_cache::clear_credit_refusal(&name, now);
    daemon.write_status();
    assert_eq!(
        lines.snapshot(),
        [refused, cleared],
        "the clear says so once"
    );

    refuse(now + 1_000);
    let mut fresh = daemon_for(config);
    fresh.write_status();
    assert_eq!(
        lines.snapshot(),
        [refused, cleared, refused],
        "a fresh daemon says a standing refusal once more"
    );
}

/// A re-login's forget leaves only a floor on the record, which reads as no
/// refusal, so the log says the refusal cleared.
#[test]
fn the_daemon_says_a_relogin_cleared_a_credit_refusal() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let config = persist(vec![blank_profile(&name)], Some("work"), 60_000);
    let mut daemon = daemon_for(config);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    crate::profile_cache::write_credit_refusal(
        &name,
        &crate::profile_cache::CreditRefusal {
            at_ms: crate::usage::now_ms(),
            file: std::path::PathBuf::from("/home/u/.claude/projects/p/conv.jsonl"),
            uuid: None,
        },
    );
    daemon.write_status();
    crate::usage::forget_account_disk(&name);
    daemon.write_status();
    assert_eq!(
        lines.snapshot(),
        [
            "work: credits refused; the global walk takes it after its headroom and home passes, \
             and a session that may need credits after every other pass, until a request on it \
             succeeds or a window reset leaves it headroom",
            "work: credit refusal cleared; auto-switch takes it again for the global account \
             and for any session",
        ]
    );
}

/// A spent usage-credit cap is said on the log at its edge: once when the
/// cached cap turns spent, once when it clears, and once more from a fresh
/// daemon over a cap still standing (the in-memory latch, like the bell's).
///
/// `write_status` is the seam: it is the per-tick work both daemon modes run,
/// so the line lands with no API listener beside it.
#[test]
fn the_daemon_says_a_spent_credit_cap_once_per_edge() {
    use crate::profile_cache::USAGE_CACHE_FILE;
    use crate::usage::{SpendInfo, UsageInfo};

    let _home = crate::testutil::HomeSandbox::new();
    // Two credit-billed families, so the ruled line's `, ` join shows.
    std::fs::write(
        crate::profile::home_dir()
            .expect("home")
            .join(".claude.json"),
        r#"{"cachedGrowthBookFeatures":{"tengu_calm_seal":["opus"]}}"#,
    )
    .expect("write the credit-billed flag");

    let name = crate::profile::ProfileName::from("work");
    let config = persist(vec![blank_profile(&name)], Some("work"), 60_000);
    let mut daemon = daemon_for(config.clone());
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let capped = || UsageInfo {
        spend: Some(SpendInfo {
            enabled: true,
            used: Some(50.0),
            limit: Some(50.0),
            percent: Some(100.0),
            currency: Some("USD".to_owned()),
        }),
        ..UsageInfo::default()
    };
    let spent = "work: credit cap spent; Claude Code refuses fable, opus on it until its \
                 monthly spend limit resets or is raised";
    let clear = "work: credit cap clear, fable, opus run again";

    crate::profile_cache::write_profile_cache(&name, USAGE_CACHE_FILE, &capped());
    daemon.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent],
        "the cap edge reaches the log, stamped families and all"
    );

    daemon.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent],
        "an unchanged tick writes nothing"
    );

    crate::profile_cache::write_profile_cache(&name, USAGE_CACHE_FILE, &UsageInfo::default());
    daemon.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent, clear],
        "the clear edge says so once"
    );

    crate::profile_cache::write_profile_cache(&name, USAGE_CACHE_FILE, &capped());
    daemon.write_status();
    let mut expected = vec![spent.to_string(), clear.to_string(), spent.to_string()];
    assert_eq!(
        lines.snapshot(),
        expected,
        "a cleared cap that fills again is a new edge"
    );

    let mut fresh = daemon_for(config);
    fresh.write_status();
    expected.push(spent.to_string());
    assert_eq!(
        lines.snapshot(),
        expected,
        "a fresh daemon over a standing cap says it once more"
    );

    fresh.write_status();
    assert_eq!(
        lines.snapshot(),
        expected,
        "and then holds, like any other unchanged tick"
    );

    // A deleted account forgets its latch, so a re-created name announces the
    // cap it still stands under (the push latch's own rule).
    daemon.config.lock().expect("config").profiles.clear();
    daemon.write_status();
    daemon
        .config
        .lock()
        .expect("config")
        .profiles
        .push(blank_profile(&name));
    daemon.write_status();
    expected.push(spent.to_string());
    assert_eq!(
        lines.snapshot(),
        expected,
        "a deleted-and-re-created account says its standing cap once more"
    );
}

/// A probe reading carries no credit figures, so it says nothing of the cap:
/// once its OAuth pair is removed and the probe reads an account whose login
/// read its cap spent, the first probe reading logs no clear. Logged in again,
/// the account says its standing cap once more.
#[test]
fn a_probe_reading_after_a_log_out_logs_no_credit_cap_clear() {
    use crate::profile_cache::USAGE_CACHE_FILE;
    use crate::usage::{SpendInfo, UsageInfo, UsageWindow};

    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("work");
    let mut profile = profile_with_creds("work", "at-work");
    profile.usage_probe = true;
    let config = persist(vec![profile.clone()], Some("work"), 90_000);
    crate::claude::write_session_token(&name, "oat-work", 0).expect("write the sidecar");
    let mut elsewhere = config.clone();
    let mut daemon = daemon_for(config);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let config_toml = crate::profile::profile_subpath(&name, "config.toml").expect("config path");
    let reading = |capped: bool| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 10.0,
            resets_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
        }),
        spend: capped.then(|| SpendInfo {
            enabled: true,
            used: Some(50.0),
            limit: Some(50.0),
            percent: Some(100.0),
            currency: Some("USD".to_owned()),
        }),
        fetched_at: Some(now_ms()),
        ..UsageInfo::default()
    };
    let probe_reads = |daemon: &Daemon| {
        daemon
            .config
            .lock()
            .expect("config")
            .find(&name)
            .is_some_and(crate::usage::usage_probe_admits)
    };
    let spent_lines = || {
        lines
            .snapshot()
            .iter()
            .filter(|line| line.starts_with("work: credit cap spent"))
            .count()
    };

    crate::profile_cache::write_profile_cache(&name, USAGE_CACHE_FILE, &reading(true));
    daemon.write_status();
    assert_eq!(spent_lines(), 1, "the login's reading logs the cap");

    crate::actions::clear_profile_credentials(&mut elsewhere, &name).expect("log out elsewhere");
    set_mtime(&config_toml, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();
    assert!(
        probe_reads(&daemon),
        "fixture control: the probe reads work now"
    );
    // As the probe lands it: windows, no credit block.
    crate::profile_cache::write_profile_cache(&name, USAGE_CACHE_FILE, &reading(false));
    daemon.write_status();
    assert!(
        lines
            .snapshot()
            .iter()
            .all(|line| !line.contains("credit cap clear")),
        "{:?}",
        lines.snapshot()
    );

    save_profile(&profile).expect("log in again elsewhere");
    set_mtime(&config_toml, SystemTime::now() + Duration::from_secs(10));
    daemon.reload_if_changed();
    assert!(
        !probe_reads(&daemon),
        "fixture control: the login reads work again"
    );
    crate::profile_cache::write_profile_cache(&name, USAGE_CACHE_FILE, &reading(true));
    daemon.write_status();
    assert_eq!(
        spent_lines(),
        2,
        "the login's reading says the standing cap once more: {:?}",
        lines.snapshot()
    );
}

/// A cache whose usage-credit cap reads spent with every window under its line
/// — the shape the credit-cap surfaces judge.
fn write_credit_capped(name: &str) {
    let usage = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 10.0,
            resets_at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
        }),
        spend: Some(crate::usage::SpendInfo {
            enabled: true,
            used: Some(50.0),
            limit: Some(50.0),
            percent: Some(100.0),
            currency: Some("USD".to_owned()),
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

/// The production path from the published body to the `credit` push: a capped
/// fresh cache under a live push daemon queues exactly one trigger across
/// ticks, off the `credit_capped` projection [`Daemon::queue_pushes`] builds.
#[test]
fn a_spent_credit_cap_queues_one_credit_push_across_ticks() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon("alpha", 10.0, crate::usage::FetchStatus::Fresh, |_, _| {});
    write_credit_capped("alpha");

    d.write_status();
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Credit {
            profile: "alpha".to_string()
        }]
    );
}

/// An account serving inference from its own endpoint is under no credit gate,
/// so a spent cap is neither logged nor pushed for it — while the same cap on
/// an account without the endpoint says both.
#[test]
fn an_own_endpoint_credit_cap_logs_nothing_and_queues_no_push() {
    let _home = HomeSandbox::new();
    std::fs::write(
        crate::profile::home_dir()
            .expect("home")
            .join(".claude.json"),
        r#"{"cachedGrowthBookFeatures":{"tengu_calm_seal":["opus"]}}"#,
    )
    .expect("write the credit-billed flag");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let spent = "plain: credit cap spent; Claude Code refuses fable, opus on it until its \
                 monthly spend limit resets or is raised";

    let (mut plain, plain_inbox) =
        push_daemon("plain", 10.0, crate::usage::FetchStatus::Fresh, |_, _| {});
    write_credit_capped("plain");
    plain.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent],
        "the control account without the endpoint says the cap"
    );
    assert_eq!(
        plain_inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Credit {
            profile: "plain".to_string()
        }],
        "and queues the push"
    );

    let (mut own, own_inbox) = push_daemon(
        "own",
        10.0,
        crate::usage::FetchStatus::Fresh,
        |profile, _| {
            profile.env.insert(
                "ANTHROPIC_BASE_URL".to_string(),
                "https://api.example.test".to_string(),
            );
            profile
                .env
                .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "tok-test".to_string());
        },
    );
    assert!(
        own.config
            .lock()
            .expect("config")
            .find(&crate::profile::ProfileName::from("own"))
            .is_some_and(crate::claude::has_own_inference_endpoint),
        "fixture control: the account serves inference on its own endpoint"
    );
    write_credit_capped("own");
    own.write_status();
    let body = std::fs::read_to_string(clauth_dir().expect("clauth dir").join("status.json"))
        .expect("read the published body");
    assert!(
        body.contains("\"credit_capped\": true"),
        "fixture control: the own-endpoint account's body still reads capped"
    );
    assert_eq!(
        lines.snapshot(),
        [spent],
        "the own-endpoint account adds no line"
    );
    assert_eq!(
        own_inbox.try_iter().collect::<Vec<_>>(),
        [],
        "and queues no push"
    );
}

/// A capped account that starts serving inference on its own endpoint is under
/// no credit gate, so both surfaces forget the announced cap; when it leaves
/// that endpoint the cap still standing is a new edge on both, not only on the
/// push.
#[test]
fn an_account_leaving_its_own_endpoint_announces_a_standing_cap_again() {
    let _home = HomeSandbox::new();
    std::fs::write(
        crate::profile::home_dir()
            .expect("home")
            .join(".claude.json"),
        r#"{"cachedGrowthBookFeatures":{"tengu_calm_seal":["opus"]}}"#,
    )
    .expect("write the credit-billed flag");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let spent = "alpha: credit cap spent; Claude Code refuses fable, opus on it until its \
                 monthly spend limit resets or is raised";

    let (mut d, inbox) = push_daemon("alpha", 10.0, crate::usage::FetchStatus::Fresh, |_, _| {});
    write_credit_capped("alpha");
    d.write_status();
    assert_eq!(lines.snapshot(), [spent], "the cap is announced first");

    {
        let mut config = d.config.lock().expect("config");
        let profile = config.profiles.first_mut().expect("the one account");
        profile.env.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            "https://api.example.test".to_string(),
        );
        profile
            .env
            .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "tok-test".to_string());
    }
    assert!(
        d.config
            .lock()
            .expect("config")
            .profiles
            .first()
            .is_some_and(crate::claude::has_own_inference_endpoint),
        "fixture control: the account serves inference on its own endpoint"
    );
    d.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent],
        "no line while the account is off the credit gate"
    );

    {
        let mut config = d.config.lock().expect("config");
        let profile = config.profiles.first_mut().expect("the one account");
        profile.env.remove("ANTHROPIC_BASE_URL");
        profile.env.remove("ANTHROPIC_AUTH_TOKEN");
    }
    d.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent, spent],
        "leaving the endpoint announces the cap still standing"
    );
    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [
            crate::push::Trigger::Credit {
                profile: "alpha".to_string()
            },
            crate::push::Trigger::Credit {
                profile: "alpha".to_string()
            },
        ],
        "and the push agrees with the log"
    );
}

/// A reading clauth distrusts is no cap edge on either surface: neither the
/// log line nor the push moves until a fresh reading lands, which then says the
/// edge exactly once — in both directions.
#[test]
fn a_stale_credit_cap_reading_logs_nothing_and_queues_no_push() {
    let _home = HomeSandbox::new();
    std::fs::write(
        crate::profile::home_dir()
            .expect("home")
            .join(".claude.json"),
        r#"{"cachedGrowthBookFeatures":{"tengu_calm_seal":["opus"]}}"#,
    )
    .expect("write the credit-billed flag");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let spent = "alpha: credit cap spent; Claude Code refuses fable, opus on it until its \
                 monthly spend limit resets or is raised";
    let clear = "alpha: credit cap clear, fable, opus run again";

    let (mut d, inbox) = push_daemon("alpha", 10.0, crate::usage::FetchStatus::Cached, |_, _| {});
    let status = |d: &mut Daemon, status: crate::usage::FetchStatus| {
        d.usage_status
            .lock()
            .expect("usage_status")
            .insert("alpha".to_string(), status);
    };
    write_credit_capped("alpha");

    d.write_status();
    assert!(
        lines.snapshot().is_empty(),
        "a stale capped reading logs nothing: {:?}",
        lines.snapshot()
    );
    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [],
        "and queues no push"
    );

    status(&mut d, crate::usage::FetchStatus::Fresh);
    d.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent],
        "the same standing cap on a fresh reading is the edge"
    );
    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Credit {
            profile: "alpha".to_string()
        }],
        "and queues the push"
    );

    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );
    status(&mut d, crate::usage::FetchStatus::Cached);
    d.write_status();
    assert_eq!(lines.snapshot(), [spent], "a stale clear logs nothing");

    status(&mut d, crate::usage::FetchStatus::Fresh);
    d.write_status();
    assert_eq!(
        lines.snapshot(),
        [spent, clear],
        "a fresh clear says so once"
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

/// The published auto-start queue reads the canceled verdict off the daemon's
/// own OAuth store: `live_stores()` clones the scheduler's `usage_store` Arc
/// into the snapshot, so a canceled plan held only there (no cache on disk)
/// drops the account and the survivor closes up.
#[test]
fn the_status_writer_drops_a_canceled_account_off_the_schedulers_own_usage_store() {
    use crate::usage::{PlanInfo, UsageInfo};
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["a", "b"]);
    let queued = |name: &str| {
        let mut p = profile_with_creds(name, &format!("{name}-access"));
        p.auto_start = true;
        p
    };
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["a".into(), "b".into()],
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![queued("a"), queued("b")],
    };
    let daemon = daemon_for(config);
    daemon.usage_store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            plan: Some(PlanInfo {
                subscription_status: Some("canceled".to_string()),
                ..PlanInfo::default()
            }),
            ..UsageInfo::default()
        },
    );

    let snapshot = daemon.live_stores().snapshot();
    let live = snapshot.signals();
    let cfg_snap = daemon.config.lock().unwrap().clone();
    let body = super::status_json::build_status(&cfg_snap, 300_000, Some(&live), false);
    let positions: Vec<(&str, Option<usize>)> = body
        .profiles
        .iter()
        .map(|p| {
            (
                p.name.as_str(),
                p.auto_start_queue.as_ref().map(|q| q.position),
            )
        })
        .collect();
    assert_eq!(
        positions,
        vec![("a", None), ("b", Some(1))],
        "the canceled account leaves the published queue and `b` closes up to 1"
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
fn an_account_deleted_from_the_roster_re_arms_the_headless_bell() {
    let _home = HomeSandbox::new();
    let mut d = bell_daemon(
        "alpha",
        90.0,
        95.0,
        crate::usage::FetchStatus::Fresh,
        Some("notify-send %s"),
    );
    d.write_status();
    // Deleted from the roster: the bell forgets it.
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles.clear();
        cfg.state.profiles.clear();
    }
    d.write_status();
    // Re-created past the line, it rings again.
    {
        let mut cfg = d.config.lock().expect("config");
        let mut profile = profile_with_creds("alpha", "at-bell");
        profile.bell_threshold = Some(90.0);
        cfg.profiles = vec![profile];
        cfg.state.profiles = vec!["alpha".into()];
    }
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
                "alert: alpha at 95%".to_string()
            ),
        ]
    );
}

/// A disabled account is hidden from the body but still on the roster, so its
/// standing bell must survive the hide: re-enabling it still past the line
/// rings nothing new. The ringing account is non-active, since the active
/// profile stays in the body even disabled.
#[test]
fn a_disabled_account_keeps_its_headless_bell_while_off_the_body() {
    let _home = HomeSandbox::new();
    RUNG.with(|r| r.borrow_mut().clear());
    let mut alpha = profile_with_creds("alpha", "at-a");
    alpha.bell_threshold = Some(90.0);
    let mut beta = profile_with_creds("beta", "at-b");
    beta.bell_threshold = Some(90.0);
    let mut config = persist(vec![alpha, beta], Some("alpha"), 60_000);
    config.state.bell_command = Some("notify-send %s".to_string());
    write_five_hour("alpha", 10.0);
    write_five_hour("beta", 95.0);
    let mut d = daemon_for(config);
    d.ring_bell = record_ring;
    for name in ["alpha", "beta"] {
        d.usage_status
            .lock()
            .expect("usage_status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }
    d.write_status();
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles
            .iter_mut()
            .find(|p| p.name.as_str() == "beta")
            .expect("beta")
            .disabled = true;
    }
    d.write_status();
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles
            .iter_mut()
            .find(|p| p.name.as_str() == "beta")
            .expect("beta")
            .disabled = false;
    }
    d.write_status();
    assert_eq!(
        rung(),
        [(
            "notify-send %s".to_string(),
            "alert: beta at 95%".to_string()
        )]
    );
}

/// The same roster semantics for the window push: a disabled account keeps its
/// window latch while off the body, so re-enabling it still past the line
/// queues no second push. The crossing account is non-active, since the active
/// profile stays in the body even disabled.
#[test]
fn a_disabled_account_keeps_its_window_latch_while_off_the_body() {
    let _home = HomeSandbox::new();
    let mut alpha = profile_with_creds("alpha", "at-a");
    alpha.fallback_threshold = Some(90.0);
    let mut beta = profile_with_creds("beta", "at-b");
    beta.fallback_threshold = Some(90.0);
    let mut config = persist(vec![alpha, beta], Some("alpha"), 60_000);
    config.state.fallback_chain = vec!["beta".into()];
    write_five_hour("alpha", 10.0);
    write_five_hour("beta", 95.0);
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    for name in ["alpha", "beta"] {
        d.usage_status
            .lock()
            .expect("usage_status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }
    d.write_status();
    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Window {
            profile: "beta".to_string()
        }]
    );
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles
            .iter_mut()
            .find(|p| p.name.as_str() == "beta")
            .expect("beta")
            .disabled = true;
    }
    d.write_status();
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles
            .iter_mut()
            .find(|p| p.name.as_str() == "beta")
            .expect("beta")
            .disabled = false;
    }
    d.write_status();
    assert_eq!(inbox.try_iter().collect::<Vec<_>>(), []);
}

/// A claude chain member deleted from the roster while past its line, then
/// re-created past the line, queues a second window push: the roster forget in
/// `queue_pushes` is what dropped its latch in between.
#[test]
fn an_account_deleted_from_the_roster_re_arms_its_window_push() {
    let _home = HomeSandbox::new();
    let mut alpha = profile_with_creds("alpha", "at-a");
    alpha.fallback_threshold = Some(90.0);
    let mut config = persist(vec![alpha], Some("alpha"), 60_000);
    config.state.fallback_chain = vec!["alpha".into()];
    write_five_hour("alpha", 95.0);
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);

    d.write_status();
    // Deleted from the roster: the forget drops its window latch.
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles.clear();
        cfg.state.profiles.clear();
        cfg.state.fallback_chain.clear();
    }
    d.write_status();
    // Re-created past the line, it queues a second push.
    {
        let mut cfg = d.config.lock().expect("config");
        let mut profile = profile_with_creds("alpha", "at-a");
        profile.fallback_threshold = Some(90.0);
        cfg.profiles = vec![profile];
        cfg.state.profiles = vec!["alpha".into()];
        cfg.state.fallback_chain = vec!["alpha".into()];
    }
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [
            crate::push::Trigger::Window {
                profile: "alpha".to_string()
            },
            crate::push::Trigger::Window {
                profile: "alpha".to_string()
            },
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

// ── push triggers ────────────────────────────────────────────────────────────

/// A daemon over one chain member `name` at line 90 reading `util` 5h under a
/// live `status`, its push inbox handed back to read.
fn push_daemon(
    name: &str,
    util: f64,
    status: crate::usage::FetchStatus,
    shape: impl FnOnce(&mut Profile, &mut AppConfig),
) -> (Daemon, std::sync::mpsc::Receiver<crate::push::Trigger>) {
    let mut profile = profile_with_creds(name, &format!("at-push-{name}"));
    profile.fallback_threshold = Some(90.0);
    let mut config = persist(vec![profile.clone()], Some(name), 60_000);
    config.state.fallback_chain = vec![name.into()];
    shape(&mut profile, &mut config);
    config.profiles = vec![profile];
    write_five_hour(name, util);
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert(name.to_string(), status);
    (d, inbox)
}

#[test]
fn a_window_reaching_the_chain_line_queues_one_push_across_ticks() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon("alpha", 95.0, crate::usage::FetchStatus::Fresh, |_, _| {});

    d.write_status();
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Window {
            profile: "alpha".to_string()
        }]
    );
}

#[test]
fn a_broken_login_queues_one_account_push_across_ticks() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon(
        "alpha",
        10.0,
        crate::usage::FetchStatus::Fresh,
        |_, config| {
            config.state.auth_broken.push("alpha".into());
        },
    );

    d.write_status();
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Account {
            profile: "alpha".to_string()
        }]
    );
}

#[test]
fn a_rejected_usage_credential_queues_an_account_push() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon(
        "alpha",
        10.0,
        crate::usage::FetchStatus::AuthExpired,
        |_, _| {},
    );

    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Account {
            profile: "alpha".to_string()
        }]
    );
}

/// A refused setup token beside a stored pair reads as a dead login on the
/// published status even while the pair's `/usage` poll is fresh, so it
/// queues the account push.
#[test]
fn a_refused_setup_token_beside_a_fresh_pair_queues_an_account_push() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon(
        "alpha",
        10.0,
        crate::usage::FetchStatus::Fresh,
        |profile, _| {
            profile.has_long_lived_sidecar = true;
            profile.setup_token_digest = Some(0x5e7);
        },
    );
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("alpha"), 0x5e7);

    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Account {
            profile: "alpha".to_string()
        }]
    );
}

#[test]
fn an_expired_access_token_waiting_on_its_rotation_queues_nothing() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon(
        "alpha",
        10.0,
        crate::usage::FetchStatus::Fresh,
        |profile, _| {
            if let Some(token) = profile
                .credentials
                .as_mut()
                .and_then(|creds| creds.claude_ai_oauth.as_mut())
            {
                token.expires_at = Some(1);
            }
        },
    );

    d.write_status();

    assert_eq!(inbox.try_iter().collect::<Vec<_>>(), []);
}

#[test]
fn a_seven_day_window_reaching_the_weekly_line_queues_one_push_across_ticks() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon(
        "alpha",
        10.0,
        crate::usage::FetchStatus::Fresh,
        |profile, _| {
            profile.weekly_threshold = Some(80.0);
        },
    );
    let resets_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let usage = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 10.0,
            resets_at: Some(resets_at.clone()),
        }),
        seven_day: Some(crate::usage::UsageWindow {
            utilization: 85.0,
            resets_at: Some(resets_at),
        }),
        fetched_at: Some(now_ms()),
        ..Default::default()
    };
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &usage,
    );

    d.write_status();
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Window {
            profile: "alpha".to_string()
        }]
    );
}

/// A monthly-only member's marked `30d` window judges at the member's weekly
/// line (the ruling's "window push follows that line too"): the published
/// `windows` array carries the 30d row and the push reads it as the long
/// window when no 7d row exists.
#[test]
fn a_monthly_window_reaching_the_weekly_line_queues_one_push_across_ticks() {
    let _home = HomeSandbox::new();
    let url = "https://token-plan.maas.qwencloudapi.com/apps/anthropic";
    let mut profile = Profile::new(
        "qwen".to_string(),
        Some(url.to_string()),
        Some("k".to_string()),
    );
    profile.provider = crate::providers::Provider::from_base_url(url);
    profile.fallback_threshold = Some(90.0);
    profile.weekly_threshold = Some(80.0);
    let mut config = persist(vec![profile.clone()], Some("qwen"), 60_000);
    config.state.fallback_chain = vec!["qwen".into()];
    config.profiles = vec![profile];
    // The marked monthly bar at 85% — past the member's own weekly line (80)
    // but under the chain default (98), so only the override can fire the push.
    // With no 5h or 7d row, only the long window could fire it at all.
    let resets_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let stats = crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![crate::providers::UsageBar {
            label: "30d".to_string(),
            pct: 85.0,
            resets_at: Some(resets_at),
            used: None,
            total: None,
            inference_quota: true,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("qwen"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("qwen".to_string(), crate::usage::FetchStatus::Fresh);

    d.write_status();
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Window {
            profile: "qwen".to_string()
        }]
    );
}

/// The control arm: the same monthly bar at 85% with NO per-account override
/// sits under the chain default (98), so no push fires. The main test's 80%
/// override is what made the bar cross — 85% clears 80 but not 98, so the two
/// arms together pin that the member's own weekly line judges the monthly
/// window, not the chain default.
#[test]
fn a_monthly_window_under_the_chain_default_queues_no_push() {
    let _home = HomeSandbox::new();
    let url = "https://token-plan.maas.qwencloudapi.com/apps/anthropic";
    let mut profile = Profile::new(
        "qwen".to_string(),
        Some(url.to_string()),
        Some("k".to_string()),
    );
    profile.provider = crate::providers::Provider::from_base_url(url);
    profile.fallback_threshold = Some(90.0);
    let mut config = persist(vec![profile.clone()], Some("qwen"), 60_000);
    config.state.fallback_chain = vec!["qwen".into()];
    config.profiles = vec![profile];
    let resets_at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let stats = crate::providers::ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![crate::providers::UsageBar {
            label: "30d".to_string(),
            pct: 85.0,
            resets_at: Some(resets_at),
            used: None,
            total: None,
            inference_quota: true,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("qwen"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("qwen".to_string(), crate::usage::FetchStatus::Fresh);

    d.write_status();
    d.write_status();

    assert!(
        inbox.try_iter().collect::<Vec<_>>().is_empty(),
        "a monthly window under the chain default queues no push"
    );
}

/// A codex chain member's 7d window is judged at the codex chain's own
/// weekly line; off that chain it is judged at none. The entries come from
/// `build_status`'s own output over a sandboxed codex cache, never hand-built.
#[test]
fn a_codex_member_crossing_the_codex_weekly_line_queues_a_window_push() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon("alpha", 10.0, crate::usage::FetchStatus::Fresh, |_, _| {});
    crate::testutil::write_codex_state(
        "profiles = [\"cx\", \"off\"]\nfallback_chain = [\"cx\"]\nweekly_switch_threshold = 80.0\n",
    );
    crate::testutil::warm_codex_usage("cx", None, 10.0, 85.0);
    crate::testutil::warm_codex_usage("off", None, 99.0, 99.0);

    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Window {
            profile: "cx".to_string()
        }]
    );
}

/// A codex chain member's 5h window is judged at the 5h line the codex chain
/// switches every member at (95, `fallback::DEFAULT_THRESHOLD`).
#[test]
fn a_codex_member_reaching_the_codex_5h_line_queues_a_window_push() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon("alpha", 10.0, crate::usage::FetchStatus::Fresh, |_, _| {});
    crate::testutil::write_codex_state("profiles = [\"cx\"]\nfallback_chain = [\"cx\"]\n");
    crate::testutil::warm_codex_usage("cx", None, 94.0, 10.0);

    d.write_status();
    let below = inbox.try_iter().collect::<Vec<_>>();

    crate::testutil::warm_codex_usage("cx", None, 95.0, 10.0);
    d.write_status();
    let at = inbox.try_iter().collect::<Vec<_>>();

    // A third tick still past the line queues nothing: the roster forget that
    // kept `cx` on its latch is what silences the standing crossing.
    d.write_status();
    let still = inbox.try_iter().collect::<Vec<_>>();

    assert_eq!(
        (below, at, still),
        (
            Vec::new(),
            vec![crate::push::Trigger::Window {
                profile: "cx".to_string()
            }],
            Vec::new(),
        )
    );
}

/// A failed codex roster read must not forget codex latches on a guess: the
/// forget runs against the last codex roster read successfully, so the
/// crossing stays latched and the restored roster re-reads silently.
#[test]
fn a_failed_codex_roster_read_does_not_forget_codex_latches() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon("alpha", 10.0, crate::usage::FetchStatus::Fresh, |_, _| {});
    crate::testutil::write_codex_state("profiles = [\"cx\"]\nfallback_chain = [\"cx\"]\n");
    crate::testutil::warm_codex_usage("cx", None, 95.0, 10.0);

    d.write_status();
    // One tick where the codex roster cannot be read: the forget must run
    // against the cached codex roster, not a claude-only one.
    crate::testutil::write_codex_state("not toml [");
    d.write_status();
    crate::testutil::write_codex_state("profiles = [\"cx\"]\nfallback_chain = [\"cx\"]\n");
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [crate::push::Trigger::Window {
            profile: "cx".to_string()
        }]
    );
}

/// A claude chain member deleted from the roster while past its line, then
/// re-created past the line, queues a second window push even while
/// `codex-profiles.toml` stays unparsable: the forget falls back to the last
/// codex roster this daemon read successfully.
#[test]
fn a_claude_account_re_arms_its_window_push_while_the_codex_roster_is_broken() {
    let _home = HomeSandbox::new();
    let mut alpha = profile_with_creds("alpha", "at-a");
    alpha.fallback_threshold = Some(90.0);
    let mut config = persist(vec![alpha], Some("alpha"), 60_000);
    config.state.fallback_chain = vec!["alpha".into()];
    write_five_hour("alpha", 95.0);
    // One tick with a readable codex roster so the daemon's last-good cache
    // fills; from then on the file stays unparsable.
    crate::testutil::write_codex_roster(&[]);
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);

    d.write_status();
    crate::testutil::write_codex_state("not toml [");
    // Deleted from the roster while the codex read fails: the forget must
    // still drop its window latch.
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles.clear();
        cfg.state.profiles.clear();
        cfg.state.fallback_chain.clear();
    }
    d.write_status();
    // Re-created past the line, it queues a second push.
    {
        let mut cfg = d.config.lock().expect("config");
        let mut profile = profile_with_creds("alpha", "at-a");
        profile.fallback_threshold = Some(90.0);
        cfg.profiles = vec![profile];
        cfg.state.profiles = vec!["alpha".into()];
        cfg.state.fallback_chain = vec!["alpha".into()];
    }
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [
            crate::push::Trigger::Window {
                profile: "alpha".to_string()
            },
            crate::push::Trigger::Window {
                profile: "alpha".to_string()
            },
        ]
    );
}

/// A daemon whose codex file was unparsable from its very first tick forgets
/// against the claude roster alone (its codex cache is empty), so a claude
/// account deleted and re-created past its line re-announces.
#[test]
fn a_claude_account_deleted_and_re_created_re_announces_while_the_codex_roster_was_never_read() {
    let _home = HomeSandbox::new();
    crate::testutil::write_codex_state("not toml [");
    let mut alpha = profile_with_creds("alpha", "at-a");
    alpha.fallback_threshold = Some(90.0);
    let mut config = persist(vec![alpha], Some("alpha"), 60_000);
    config.state.fallback_chain = vec!["alpha".into()];
    write_five_hour("alpha", 95.0);
    let mut d = daemon_for(config);
    let (queue, inbox) = crate::push::queue_for_tests();
    d.push = Some(queue);
    d.usage_status
        .lock()
        .expect("usage_status")
        .insert("alpha".to_string(), crate::usage::FetchStatus::Fresh);

    d.write_status();
    {
        let mut cfg = d.config.lock().expect("config");
        cfg.profiles.clear();
        cfg.state.profiles.clear();
        cfg.state.fallback_chain.clear();
    }
    d.write_status();
    {
        let mut cfg = d.config.lock().expect("config");
        let mut profile = profile_with_creds("alpha", "at-a");
        profile.fallback_threshold = Some(90.0);
        cfg.profiles = vec![profile];
        cfg.state.profiles = vec!["alpha".into()];
        cfg.state.fallback_chain = vec!["alpha".into()];
    }
    d.write_status();

    assert_eq!(
        inbox.try_iter().collect::<Vec<_>>(),
        [
            crate::push::Trigger::Window {
                profile: "alpha".to_string()
            },
            crate::push::Trigger::Window {
                profile: "alpha".to_string()
            },
        ]
    );
}

/// A codex chain that names a claude profile is not a codex membership: the
/// harness check keeps the claude entry off the codex line, so nothing queues.
#[test]
fn a_codex_chain_naming_a_claude_profile_queues_nothing_for_it() {
    let _home = HomeSandbox::new();
    let (mut d, inbox) = push_daemon(
        "alpha",
        95.0,
        crate::usage::FetchStatus::Fresh,
        |_, config| {
            config.state.fallback_chain.clear();
        },
    );
    crate::testutil::write_codex_state("profiles = []\nfallback_chain = [\"alpha\"]\n");

    d.write_status();
    d.write_status();

    assert_eq!(inbox.try_iter().collect::<Vec<_>>(), []);
}

// ── T33b1: the daemon holds the per-account background tree ────────────────

#[cfg(target_os = "linux")]
/// The background pair of `name`, as a launch leaves it: both dirs exist and
/// a Claude Code job record (state `state`) names the tree. Returns
/// `(tree, sessions dir, job dir)`. Arms both heal throttles, since every
/// caller ticks a daemon and neither heal is what these tests are about.
fn background_pair(
    sb: &HomeSandbox,
    name: &str,
    state: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let profile =
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let tree = profile.join("runtime-bg");
    let sessions = profile.join("sessions-bg");
    std::fs::create_dir_all(&tree).expect("mkdir tree");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let job = crate::testutil::write_claude_job_in(sb.home(), "b6a1c0de", &tree, state, None);
    (tree, sessions, job)
}

/// Watchdog threads this process runs for `name`, read off the kernel's
/// thread names (`comm`, cut to 15 bytes), so the count needs no daemon
/// accessor.
#[cfg(target_os = "linux")]
fn watchdog_threads(name: &str) -> usize {
    let mut want = format!("clauth-wdog-{name}");
    want.truncate(15);
    std::fs::read_dir("/proc/self/task")
        .expect("task dir")
        .flatten()
        .filter(|task| {
            let Some(comm) = read_task_file(&task.path(), "comm") else {
                return false;
            };
            comm.trim_end() == want && !task_is_exiting(&task.path())
        })
        .count()
}

/// One `/proc/self/task/<tid>/<file>`, or `None` once the task is gone (not
/// found, no such process). Any other read failure panics, so a full fd table
/// never hides a live thread from a count.
#[cfg(target_os = "linux")]
fn read_task_file(task: &std::path::Path, file: &str) -> Option<String> {
    match std::fs::read_to_string(task.join(file)) {
        Ok(text) => Some(text),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(libc::ESRCH) =>
        {
            None
        }
        Err(e) => panic!("cannot read {}: {e}", task.join(file).display()),
    }
}

/// Whether a `/proc/self/task/<tid>` entry is a thread already past its exit:
/// a joined thread stays listed for a moment with `PF_EXITING` (0x4) set in
/// its `stat` flags (field 9), so counting it would read a joined watchdog as
/// alive. A task gone by the time `stat` is read counts as exiting.
#[cfg(target_os = "linux")]
fn task_is_exiting(task: &std::path::Path) -> bool {
    const PF_EXITING: u64 = 0x4;
    let Some(stat) = read_task_file(task, "stat") else {
        return true;
    };
    // Fields after the parenthesised `comm`, which may itself hold spaces:
    // state is field 3, so flags (field 9) is the 7th after the `)`.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return true;
    };
    rest.split_whitespace()
        .nth(6)
        .and_then(|flags| flags.parse::<u64>().ok())
        .is_none_or(|flags| flags & PF_EXITING != 0)
}

#[cfg(target_os = "linux")]
/// The `live_sessions` rows that start on `name`.
fn rows_of(name: &str) -> Vec<crate::live_sessions::LiveSession> {
    crate::live_sessions::list()
        .into_iter()
        .filter(|row| row.start_profile == name)
        .collect()
}

/// Tick until `done` holds, failing after 20 s with `what`.
#[cfg(target_os = "linux")]
fn tick_until(daemon: &mut Daemon, what: &str, mut done: impl FnMut(&Daemon) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let probes = crate::runtime::link_mode_probe_calls();
    loop {
        daemon.tick();
        assert_eq!(
            crate::runtime::link_mode_probe_calls(),
            probes,
            "the tick must never probe the link mode; the adoption worker probes under the lock"
        );
        if done(daemon) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "never {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Tick until no adoption is queued or in flight, failing after 20 s: an
/// adoption the worker still runs after a test drops its forced link mode would
/// see the real one and could adopt behind the test's back.
#[cfg(target_os = "linux")]
fn tick_until_no_adoption_pending(daemon: &mut Daemon) {
    tick_until(daemon, "settled its queued adoptions", |d| {
        d.background.pending.is_empty()
    });
}

/// Whether the daemon holds `name`'s background tree: the hold is in the
/// daemon's own map (the worker's acquire alone lands the marker, row and
/// watchdog before the tick collects the runtime, and a daemon dropped before
/// the worker hands the runtime back leaves its teardown to the worker), a
/// held marker in `sessions-bg/`, one row naming that marker, one watchdog.
#[cfg(target_os = "linux")]
fn adopted(daemon: &Daemon, name: &str, sessions: &std::path::Path) -> bool {
    let account = crate::profile::ProfileName::from(name);
    let rows = rows_of(name);
    daemon
        .background
        .held
        .contains_key(&super::BackgroundKey::Account(account.clone()))
        && crate::runtime::has_live_marker(&account)
        && rows.len() == 1
        && sessions.join(&rows[0].session_id).is_file()
        && watchdog_threads(name) == 1
}

/// T33b1 check 4: a tick that finds a job naming an account's background tree
/// with no daemon hold adopts it: a marker it holds in `sessions-bg/`, a
/// `live_sessions` row naming that marker, the tree's watchdog. The tick
/// thread never waits for it: with the account's rotation lock and the state
/// flock held elsewhere, the tick returns at once and the adoption lands once
/// they free.
#[cfg(target_os = "linux")]
#[test]
fn a_tick_adopts_a_background_tree_a_job_names_without_waiting_on_its_locks() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (_tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);

    let (rot_held_tx, rot_held_rx) = std::sync::mpsc::channel();
    let (rot_release_tx, rot_release_rx) = std::sync::mpsc::channel::<()>();
    let rotation_holder = std::thread::spawn(move || {
        let rotation = crate::testutil::hold_rotation_lock("bgp");
        rot_held_tx.send(()).expect("report the rotation hold");
        // Release after the tick's 5 s budget even if nothing signals, so a
        // tick that waits on the rotation lock returns and the elapsed
        // assertion below fails instead of the test hanging.
        let _ = rot_release_rx.recv_timeout(Duration::from_secs(6));
        drop(rotation);
    });
    rot_held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the rotation lock is held");

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let flock_holder = std::thread::spawn(move || {
        crate::lock::with_state_lock(|_| {
            held_tx.send(()).expect("report the hold");
            // Release after the tick's 5 s budget even if nothing signals, so
            // a tick that waits on the flock returns and the elapsed
            // assertion below fails instead of the test hanging.
            let _ = release_rx.recv_timeout(Duration::from_secs(6));
            Ok(())
        })
        .expect("hold the state flock");
    });
    held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the state flock is held");

    let started = std::time::Instant::now();
    daemon.tick();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the tick waited {took:?} on a lock the adoption needs"
    );

    release_tx.send(()).expect("release the flock");
    flock_holder.join().expect("flock holder");
    rot_release_tx.send(()).expect("release the rotation lock");
    rotation_holder.join().expect("rotation holder");
    tick_until(&mut daemon, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });
}

/// T33b1 check 5: a fresh daemon re-adopts the tree while the job still names
/// it; the first daemon's teardown spares the tree and frees its hold.
#[cfg(target_os = "linux")]
#[test]
fn a_fresh_daemon_re_adopts_the_background_tree_while_a_job_names_it() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut first = daemon_for(config);
    tick_until(&mut first, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });

    drop(first);
    assert!(tree.is_dir(), "the job keeps the tree past the daemon");
    assert!(
        !crate::runtime::has_live_marker(&crate::profile::ProfileName::from("bgp")),
        "the old daemon's marker died with it"
    );
    assert_eq!(
        watchdog_threads("bgp"),
        0,
        "the old daemon's watchdog is no longer running"
    );

    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let mut second = daemon_for(config);
    tick_until(&mut second, "re-adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });
}

/// T33b1 check 6, the daemon half: once the last job naming the tree is gone
/// (`claude rm` deletes its job dir), the daemon drops its hold and the pair
/// is collected: no marker, no row, no watchdog, no tree.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_releases_the_background_tree_once_its_last_job_is_removed() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, sessions, job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });

    std::fs::remove_dir_all(&job).expect("claude rm");
    tick_until(&mut daemon, "released the background tree", |_| {
        !tree.exists() && !sessions.exists()
    });
    assert_eq!(rows_of("bgp").len(), 0, "the row went with the hold");
    assert_eq!(watchdog_threads("bgp"), 0, "no watchdog is still running");
}

/// T33b1 N1 (cloudy 2026-10-22, option A), the daemon half: on a host that
/// copies runtime trees the daemon never adopts a background tree, since its
/// copy mirror would republish a removed job's record. The tree itself is
/// left alone (a job still names it). Control: the same fixture adopts once
/// the host links again, so the window is no slow adoption.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_never_adopts_a_background_tree_on_a_copy_host() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);
    let name = crate::profile::ProfileName::from("bgp");

    let fake_mode = crate::runtime::ForcedFakeLinkMode::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let probes = crate::runtime::link_mode_probe_calls();
    while std::time::Instant::now() < deadline {
        daemon.tick();
        assert_eq!(
            crate::runtime::link_mode_probe_calls(),
            probes,
            "the tick must never probe the link mode; the adoption worker probes under the lock"
        );
        assert!(
            !crate::runtime::has_live_marker(&name),
            "the daemon claimed a marker on a copy host"
        );
        assert_eq!(
            rows_of("bgp").len(),
            0,
            "a row was registered on a copy host"
        );
        assert_eq!(
            watchdog_threads("bgp"),
            0,
            "a watchdog started on a copy host"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(tree.is_dir(), "the job's tree is left alone");
    tick_until_no_adoption_pending(&mut daemon);
    assert!(
        daemon
            .background
            .copy_host
            .contains_key(&super::BackgroundKey::Account(name)),
        "the copy host refused the account before the host links"
    );
    drop(fake_mode);

    tick_until(
        &mut daemon,
        "adopted the background tree once the host links",
        |d| adopted(d, "bgp", &sessions),
    );
}

/// F10: on a copy host the in-lock floor's refusal is a designed skip, not an
/// adoption failure: the daemon logs it once and re-queues the account at most
/// once (one re-check, so a link-mode flip is adopted), so no retry streak
/// re-takes the rotation lock and the state flock every interval while a job
/// names the tree. The `adoption_attempts` seam pins exactly two queues past
/// the 10 s re-check.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_logs_the_copy_host_refusal_once_and_requeues_at_most_once() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (_tree, _sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);
    let name = crate::profile::ProfileName::from("bgp");

    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    let fake_mode = crate::runtime::ForcedFakeLinkMode::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        daemon.tick();
        assert!(
            !crate::runtime::has_live_marker(&name),
            "the daemon claimed a marker on a copy host"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    tick_until_no_adoption_pending(&mut daemon);
    drop(fake_mode);
    drop(capture);

    let snap = lines.snapshot();
    let refusals: Vec<&str> = snap
        .iter()
        .map(String::as_str)
        .filter(|line| line.contains("will not adopt 'bgp'"))
        .collect();
    assert_eq!(
        refusals,
        vec![
            "clauth daemon: will not adopt 'bgp' background tree on this host: it \
             copies runtime trees instead of linking them"
        ],
        "the copy-host refusal is logged exactly once, with the skip copy"
    );
    assert!(
        !snap
            .iter()
            .any(|line| line.contains("adopting 'bgp' background tree failed")),
        "the copy-host refusal never reads as an adoption failure"
    );
    assert_eq!(
        daemon.background.adoption_attempts, 2,
        "one adoption attempt plus exactly one re-check, never a backoff retry"
    );
}

/// A NEW job naming the tree re-arms the copy-host re-check after the account's
/// job set emptied: the copy-host gate is pruned beside the re-check timer, so
/// a later `--bg` job (after Developer Mode is turned on, say) gets one fresh
/// attempt instead of being skipped for the daemon's life.
#[cfg(target_os = "linux")]
#[test]
fn a_new_background_job_re_arms_the_copy_host_recheck() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, _sessions, job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);

    let fake_mode = crate::runtime::ForcedFakeLinkMode::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        daemon.tick();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        daemon.background.adoption_attempts, 2,
        "one adoption attempt plus exactly one re-check on a copy host"
    );

    // The job set empties: the copy-host gate and its timer are pruned, so the
    // next job naming the tree gets one fresh attempt.
    std::fs::remove_dir_all(&job).expect("claude rm");
    daemon.tick();
    crate::testutil::write_claude_job_in(sb.home(), "c0ffee00", &tree, "working", None);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && daemon.background.adoption_attempts < 3 {
        daemon.tick();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        daemon.background.adoption_attempts, 3,
        "a new job re-arms the copy-host re-check, exactly once"
    );
    tick_until_no_adoption_pending(&mut daemon);
    drop(fake_mode);
}

/// A NEW job beside an OLDER one (the job-id set changed without emptying)
/// re-arms the copy-host re-check, so once the host links and the set gains a
/// job, the daemon adopts within the re-check window instead of skipping the
/// account until a daemon restart.
#[cfg(target_os = "linux")]
#[test]
fn a_new_job_beside_an_older_one_re_arms_the_copy_host_recheck() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);

    // Copy host until the one re-check is spent, then link again.
    let fake_mode = crate::runtime::ForcedFakeLinkMode::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while std::time::Instant::now() < deadline && daemon.background.adoption_attempts < 2 {
        daemon.tick();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        daemon.background.adoption_attempts, 2,
        "one adoption attempt plus exactly one re-check on a copy host"
    );
    tick_until_no_adoption_pending(&mut daemon);
    assert!(
        !adopted(&daemon, "bgp", &sessions),
        "the copy host refused both attempts before the host links"
    );
    drop(fake_mode);

    crate::testutil::write_claude_job_in(sb.home(), "c0ffee00", &tree, "working", None);
    tick_until(
        &mut daemon,
        "adopted the background tree once the host links and a new job joins \
         an older one still naming it",
        |d| adopted(d, "bgp", &sessions),
    );
}

/// An account whose profile dir is unreadable must not queue an adoption: the
/// build reads its config and probes its link mode under that dir, so queueing
/// it would start a failing backoff streak that re-takes the locks every
/// interval. Drives the reconcile pass directly: the full tick's config reload
/// re-tightens a chmod-000 profile dir the daemon owns, so only a direct
/// reconcile pins the scoped unreadable the fix skips.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_does_not_retry_an_account_whose_profile_dir_is_unreadable() {
    use std::os::unix::fs::PermissionsExt as _;
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let profile =
        crate::profile::profile_dir(&crate::profile::ProfileName::from("bgp")).expect("dir");
    // A leftover background pair with no job: the unreadable profile dir reads
    // as "spares" fail-closed, so without the skip it would queue an adoption.
    std::fs::create_dir_all(profile.join("runtime-bg")).expect("mkdir tree");
    std::fs::create_dir_all(profile.join("sessions-bg")).expect("mkdir sessions");
    let mut daemon = daemon_for(config);

    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::read_dir(&profile).is_ok() {
        let _ = std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700));
        return;
    }
    for _ in 0..10 {
        daemon
            .background
            .reconcile(vec![crate::profile::ProfileName::from("bgp")], Vec::new());
        assert_eq!(
            daemon.background.adoption_attempts, 0,
            "an unreadable profile dir must not queue an adoption"
        );
    }
    let _ = std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700));
}

/// One record-less dir in the GLOBAL store — a `claude --bg` caught mid-create,
/// or a crash's leftover — makes the store read `Named` with a global
/// mid-create verdict, not `Unreadable`. The gates keep sparing, but nothing
/// says a job names any account's tree, so the adoption leg must leave the
/// holds as they are and adopt NOTHING: without the skip it would build a
/// background tree, claim a marker and write a hold row for EVERY configured
/// account off one stray dir, permanently.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_adopts_nothing_while_the_global_store_holds_a_recordless_dir() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let config = persist(
        vec![
            profile_with_creds("bg1", "tok-1"),
            profile_with_creds("bg2", "tok-2"),
            profile_with_creds("bg3", "tok-3"),
        ],
        None,
        60_000,
    );
    let mut daemon = daemon_for(config);
    std::fs::create_dir_all(
        crate::profile::claude_dir()
            .expect("claude dir")
            .join("jobs")
            .join("0add99f3"),
    )
    .expect("mkdir the record-less dir");

    for _ in 0..10 {
        daemon.background.reconcile(
            vec![
                crate::profile::ProfileName::from("bg1"),
                crate::profile::ProfileName::from("bg2"),
                crate::profile::ProfileName::from("bg3"),
            ],
            Vec::new(),
        );
        assert_eq!(
            daemon.background.adoption_attempts, 0,
            "a global mid-create store adopts nothing: no job names any account"
        );
    }
}

/// The daemon's tick thread must never probe the link mode: the probe writes
/// shared dotfiles, so a tick-side probe would race every locked `clauth start`
/// probe on the same account. The adoption worker probes under the state lock
/// instead; this pins the tick through a thread-local call counter.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_tick_never_probes_the_link_mode() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (_tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);

    let before = crate::runtime::link_mode_probe_calls();
    daemon.tick();
    assert_eq!(
        crate::runtime::link_mode_probe_calls(),
        before,
        "the tick must never probe the link mode; the adoption worker probes under the lock"
    );
    tick_until(&mut daemon, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });
}

/// T33b1 check 8: on macOS the daemon's background hold alone never blocks a
/// rotation (a stopped job is not mid-task); an unfinished job on the tree
/// does, through the job term.
#[cfg(target_os = "linux")]
#[test]
fn the_daemons_background_hold_alone_never_blocks_a_macos_rotation() {
    let sb = HomeSandbox::new();
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, sessions, job) = background_pair(&sb, "bgp", "stopped");
    let name = crate::profile::ProfileName::from("bgp");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });

    assert!(
        !crate::runtime::rotation_blocked_on(&name, true),
        "the daemon's hold over a stopped job's tree leaves rotation free"
    );
    std::fs::remove_dir_all(&job).expect("drop the stopped job");
    crate::testutil::write_claude_job_in(sb.home(), "c7d2e1f0", &tree, "working", None);
    assert!(
        crate::runtime::rotation_blocked_on(&name, true),
        "a working job on the tree blocks rotation"
    );
}

/// T33b1 check 9: the adopted tree is shared-flavour, so the
/// `sessions/<pid>.json` record a background session writes through it lands
/// in the global store, where the bridge resolver looks for a pid no row
/// joins.
#[cfg(target_os = "linux")]
#[test]
fn an_adopted_background_tree_writes_session_records_to_the_global_store() {
    let sb = HomeSandbox::new();
    std::fs::create_dir_all(claude_dir().expect("claude dir").join("sessions"))
        .expect("global sessions store");
    let config = persist(vec![profile_with_creds("bgp", "tok-bgp")], None, 60_000);
    let (tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });

    let record = tree.join("sessions").join("4242.json");
    std::fs::write(&record, b"{}").expect("a record written through the tree");
    assert!(
        claude_dir()
            .expect("claude dir")
            .join("sessions")
            .join("4242.json")
            .is_file(),
        "the record is in the global store"
    );
}

/// A peer's switch after the daemon's config read stamps the account it made
/// active into the live settings: the background hold the daemon queues
/// strips that account's custom env too, read off disk, so the adopted tree's
/// settings never carry it.
#[cfg(target_os = "linux")]
#[test]
fn a_background_hold_strips_the_env_a_peer_switch_stamped_after_the_daemons_read() {
    let sb = HomeSandbox::new();
    let mut peer = profile_with_creds("peer", "tok-peer");
    peer.env
        .insert("PEER_ONLY_KEY".to_string(), "peer-secret".to_string());
    let config = persist(
        vec![
            profile_with_creds("bgp", "tok-bgp"),
            profile_with_creds("work", "tok-work"),
            peer,
        ],
        Some("work"),
        60_000,
    );
    link_active_clean("work");
    let (tree, sessions, _job) = background_pair(&sb, "bgp", "working");
    let mut daemon = daemon_for(config);

    let mut peer_config = crate::profile::load_config().expect("peer read");
    assert!(
        crate::actions::switch_profile_locked(
            &mut peer_config,
            &crate::profile::ProfileName::from("peer")
        )
        .expect("peer switch"),
        "fixture: the peer switched"
    );
    assert!(
        crate::claude::claude_settings_env_keys()
            .expect("live settings")
            .contains(&"PEER_ONLY_KEY".to_string()),
        "fixture: the peer's switch stamped its key live"
    );

    // The daemon's own read still names `work`: the hold is queued off it,
    // before any tick reloads the config.
    daemon.reconcile_background_holds();
    tick_until(&mut daemon, "adopted the background tree", |d| {
        adopted(d, "bgp", &sessions)
    });

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tree.join("settings.json")).expect("the tree's settings"),
    )
    .expect("parse settings");
    assert!(
        settings["env"].get("PEER_ONLY_KEY").is_none(),
        "the peer-switched account's env must not ride into the background tree: {settings}"
    );
}

/// The tick logs a strip-list read that fails once per kind of failure, not
/// once per tick.
#[test]
fn a_failing_strip_list_read_logs_once_per_kind() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("work", "tok-work")],
        Some("work"),
        60_000,
    );
    let mut daemon = daemon_for(config);
    let state = clauth_dir().expect("clauth dir").join("profiles.toml");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    std::fs::write(&state, "this is = = not toml").expect("tear the roster");
    daemon.reconcile_background_holds();
    daemon.reconcile_background_holds();
    let torn = lines.snapshot();
    assert_eq!(torn.len(), 1, "one line for two failing reads: {torn:?}");
    assert!(
        torn[0].starts_with(
            "clauth daemon: could not re-read the active account's env keys, background \
             sessions use the loaded copy: "
        ),
        "{torn:?}"
    );
    std::fs::remove_file(&state).expect("drop the roster");
    std::fs::create_dir(&state).expect("a dir where the roster goes");
    daemon.reconcile_background_holds();
    daemon.reconcile_background_holds();
    assert_eq!(
        lines.snapshot().len(),
        2,
        "a new kind of failure logs once more"
    );
}

/// F6: a worker that died (a panic) is reported once and stops all queueing, so
/// nothing falls back to running an acquire or a teardown on the tick thread.
#[cfg(target_os = "linux")]
#[test]
fn a_lost_background_hold_worker_is_reported_and_stops_the_tick_queueing() {
    let _home = HomeSandbox::new();
    let mut holds = super::BackgroundHolds::new();
    // Closing the only work sender ends the worker loop; its done sender drops
    // with it, which is exactly the shape a panic leaves.
    drop(holds.work.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !holds.worker_gone {
        assert!(
            std::time::Instant::now() < deadline,
            "the lost worker was never detected"
        );
        holds.reconcile(Vec::new(), Vec::new());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(holds.work.is_none(), "no sender is left to hand work to");
    assert!(holds.pending.is_empty(), "no adoption stays in flight");
    // A tick that would otherwise adopt must not panic or queue anything.
    holds.reconcile(vec![crate::profile::ProfileName::from("bgp")], Vec::new());
    assert!(holds.held.is_empty());
}

/// F7: a failing adoption backs off per account and its line logs once per
/// streak, not once per tick.
#[test]
fn an_adoption_failure_backs_off_and_logs_once_per_streak() {
    let mut retry = std::collections::HashMap::new();
    let key = super::BackgroundKey::Account(crate::profile::ProfileName::from("bgp"));
    let first = super::note_adoption_failure(&mut retry, &key, "boom", 1_000)
        .expect("the first failure of a streak is news");
    assert!(
        first.contains("adopting 'bgp' background tree failed"),
        "the line names the account: {first}"
    );
    assert_eq!(
        retry.get(&key).map(|failure| failure.not_before_ms),
        Some(1_000 + super::adopt_backoff_ms(1))
    );
    assert!(
        super::note_adoption_failure(&mut retry, &key, "boom", 20_000).is_none(),
        "an identical failure repeating on the schedule stays silent"
    );
    assert!(
        super::note_adoption_failure(&mut retry, &key, "different", 40_000).is_some(),
        "a different failure is news"
    );
}

/// F7/N7: the first wait is the stated minimum, the rest doubles, and the cap
/// holds.
#[test]
fn the_adoption_backoff_doubles_from_a_floor_and_caps() {
    assert_eq!(
        super::adopt_backoff_ms(1),
        super::ADOPT_BACKOFF_MIN_MS,
        "one failure waits the stated minimum"
    );
    assert_eq!(super::adopt_backoff_ms(2), super::ADOPT_BACKOFF_MIN_MS * 2);
    assert_eq!(super::adopt_backoff_ms(3), super::ADOPT_BACKOFF_MIN_MS * 4);
    assert_eq!(
        super::adopt_backoff_ms(u32::MAX),
        super::ADOPT_BACKOFF_MAX_MS,
        "the cap holds however long the streak runs"
    );
}

/// N4: a release that cannot be handed to the worker puts the runtime back under
/// its hold — the marker stays held, and only the daemon's own drop may tear it
/// down. The receiver is dropped while a sender still lives, the exact window
/// `hold_back` covers.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_release_send_puts_the_runtime_back_under_its_hold() {
    let sb = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("bghold");
    persist(
        vec![profile_with_creds("bghold", "tok-bghold")],
        None,
        60_000,
    );
    let (_tree, sessions, job) = background_pair(&sb, "bghold", "working");
    let runtime = crate::runtime::ProfileRuntime::acquire_background(
        &name,
        crate::runtime::BackgroundHold::Daemon,
        &[],
        None,
    )
    .expect("adopt the background tree");
    let marker = crate::live_sessions::list()
        .into_iter()
        .find(|row| row.start_profile == "bghold")
        .map(|row| sessions.join(row.session_id))
        .expect("the hold registered a row");
    assert!(
        marker.is_file(),
        "the hold claims its marker before release"
    );

    let mut holds = super::BackgroundHolds::new();
    holds
        .held
        .insert(super::BackgroundKey::Account(name.clone()), runtime);
    // The worker is gone: its work receiver is closed and its done channel can
    // never report anything, so the tick reaches the release it cannot hand off.
    let (work_tx, work_rx) = crossbeam_channel::unbounded::<super::BackgroundWork>();
    drop(work_rx);
    holds.work = Some(work_tx);
    holds.done = crossbeam_channel::never();
    std::fs::remove_dir_all(&job).expect("the job is gone from the store");

    holds.reconcile(Vec::new(), Vec::new());

    assert!(holds.worker_gone, "the dead worker is detected");
    assert!(
        holds
            .held
            .contains_key(&super::BackgroundKey::Account(name)),
        "a release that could not be handed off stays under its hold"
    );
    assert!(
        marker.is_file(),
        "the hold's marker is not torn down on the tick thread"
    );
}

/// N5/N7: the tick's own backoff gate. A failing adoption is not retried inside
/// its window, and its streak goes when the job that named the tree does.
#[cfg(target_os = "linux")]
#[test]
fn a_failing_adoption_backs_off_on_the_tick_and_its_streak_clears_with_the_job() {
    let sb = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("bgflap");
    let (_tree, _sessions, job) = background_pair(&sb, "bgflap", "working");
    persist(
        vec![profile_with_creds("bgflap", "tok-bgflap")],
        None,
        60_000,
    );
    // Every adoption attempt fails deterministically, before it holds anything.
    std::fs::write(
        crate::profile::profile_dir(&name)
            .expect("profile dir")
            .join("config.toml"),
        "[[[",
    )
    .expect("corrupt the profile config");

    let key = super::BackgroundKey::Account(name.clone());
    let mut holds = super::BackgroundHolds::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while holds
        .retry
        .get(&key)
        .is_none_or(|failure| failure.attempts == 0)
    {
        holds.reconcile(vec![name.clone()], Vec::new());
        assert!(
            std::time::Instant::now() < deadline,
            "the failing adoption was never recorded"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        holds.retry.get(&key).map(|failure| failure.attempts),
        Some(1),
        "one failing attempt is one failure"
    );
    // Hold the window open for the whole loop, so the gate (not the clock) is
    // what the next assertion measures.
    holds
        .retry
        .get_mut(&key)
        .expect("the streak was recorded")
        .not_before_ms = u64::MAX;

    for _ in 0..100 {
        holds.reconcile(vec![name.clone()], Vec::new());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        holds.retry.get(&key).map(|failure| failure.attempts),
        Some(1),
        "a tick inside the backoff window does not retry the adoption"
    );

    std::fs::remove_dir_all(&job).expect("the job is gone from the store");
    holds.reconcile(vec![name.clone()], Vec::new());
    assert!(
        !holds.retry.contains_key(&key),
        "a streak whose job has gone must not delay the next adoption"
    );
}

// ── the daemon adopts a `--with-fallback` background session ────────
//
// Every `--with-fallback` session a Claude Code job outlives is handed to the
// daemon: an explicit `-- --bg` launch, and a foreground
// session moved to the background from its terminal. The launch account is
// `wfl`, the chain `wfl` -> `wfs`.

/// The chain this section launches on: `wfl` -> `wfs`, both OAuth logins with
/// live tokens, persisted where `acquire` and the swap executor re-read them.
/// Arms both heal throttles, since every caller ticks a daemon and neither
/// heal is what these tests are about.
#[cfg(target_os = "linux")]
fn with_fallback_chain() -> AppConfig {
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let mut config = persist(
        vec![
            profile_with_creds("wfl", "tok-wfl"),
            profile_with_creds("wfs", "tok-wfs"),
        ],
        None,
        60_000,
    );
    config.state.fallback_chain = vec!["wfl".into(), "wfs".into()];
    save_app_state(&config.state).expect("persist the chain");
    config
}

/// The sandbox's `~/.claude`, pinned as the config dir `clauth start` links
/// its runtime trees' `jobs/` and `projects/` into.
#[cfg(target_os = "linux")]
fn with_fallback_claude_home(sb: &HomeSandbox) -> crate::testutil::ConfigDirSandbox<'_> {
    let claude = sb.home().join(".claude");
    std::fs::create_dir_all(&claude).expect("~/.claude");
    crate::testutil::ConfigDirSandbox::new(sb, &claude)
}

/// What `clauth start wfl --with-fallback <args>` leaves once its launcher
/// exits, driven through the real start path. The fake `claude` answers both
/// `--bg` and a `/background` prompt by writing a job record through its
/// config dir and exiting at once, which is what the launcher sees of either.
/// A held daemon lock stands in for the running daemon `--with-fallback`
/// demands, and is released before this returns. Returns the launch's tree and
/// session id.
#[cfg(target_os = "linux")]
fn launch_with_fallback(
    fake: &crate::testutil::FakeClaude<'_>,
    config: &AppConfig,
    arg: &str,
) -> (std::path::PathBuf, String) {
    let daemon_lock = crate::daemon::hold_daemon_lock();
    crate::start::run(
        config,
        &crate::profile::ProfileName::from("wfl"),
        &[arg.to_string()],
        crate::runtime::Isolation::Shared,
        None,
        true,
        None,
    )
    .expect("a --with-fallback start");
    drop(daemon_lock);
    let dirs: Vec<String> = fake
        .spawns()
        .into_iter()
        .filter(|(_, argv)| argv.split(' ').any(|word| word == arg))
        .map(|(dir, _)| dir)
        .collect();
    assert_eq!(dirs.len(), 1, "one launch");
    let tree = std::path::PathBuf::from(&dirs[0]);
    let sid = tree
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("runtime-"))
        .expect("a --with-fallback launch runs on its own per-launch tree")
        .to_string();
    assert!(tree.is_dir(), "the job keeps the tree past its launcher");
    (tree, sid)
}

/// The job record in the sandbox's global store that names `tree`.
#[cfg(target_os = "linux")]
fn job_naming(sb: &HomeSandbox, tree: &std::path::Path) -> std::path::PathBuf {
    let jobs = sb.home().join(".claude").join("jobs");
    let named: Vec<std::path::PathBuf> = std::fs::read_dir(&jobs)
        .expect("the launch left a job")
        .flatten()
        .map(|entry| entry.path())
        .filter(|job| {
            std::fs::read(job.join("state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|record| {
                    record["providerEnv"]["CLAUDE_CONFIG_DIR"]
                        .as_str()
                        .map(std::path::PathBuf::from)
                })
                .is_some_and(|dir| dir == tree)
        })
        .collect();
    assert_eq!(named.len(), 1, "one job names {}", tree.display());
    named.into_iter().next().expect("one job")
}

/// The marker `sid` keeps under `member`'s profile dir.
#[cfg(target_os = "linux")]
fn with_fallback_marker(member: &str, sid: &str) -> std::path::PathBuf {
    crate::profile::profile_dir(&crate::profile::ProfileName::from(member))
        .expect("profile dir")
        .join(format!("sessions-{sid}"))
        .join(sid)
}

/// Whether `daemon` holds session `sid` running on `member`: the hold is in
/// the daemon's own map, its row names this pid, still follows the chain, and
/// a marker of `sid` under `member` is held. The launcher ran in this same
/// process, so the pid alone proves nothing here; the held marker is what
/// does, since the launcher's died with its teardown. The map is checked too:
/// the worker's adoption lands the marker and the row before the tick collects
/// the runtime, and a daemon dropped in that window leaves the teardown to a
/// worker that can outlive the test's sandbox.
#[cfg(target_os = "linux")]
fn daemon_holds_session(daemon: &Daemon, sid: &str, member: &str) -> bool {
    daemon
        .background
        .held
        .contains_key(&super::BackgroundKey::Session(sid.to_string()))
        && crate::live_sessions::get(sid).is_some_and(|row| {
            row.pid == std::process::id()
                && row.follows_chain
                && crate::runtime::session_row_is_live(
                    &crate::profile::ProfileName::from(member),
                    false,
                    sid,
                )
        })
}

/// The store `member`'s credential link resolves to, canonicalized: the
/// gate's TMPDIR is a symlink, so a raw spelling never compares.
#[cfg(target_os = "linux")]
fn member_store_of(member: &str) -> std::path::PathBuf {
    let store = crate::claude::install_source_path(&crate::profile::ProfileName::from(member))
        .expect("install source");
    std::fs::canonicalize(store).expect("the member's store exists")
}

/// What the tree's `.credentials.json` resolves to, canonicalized.
#[cfg(target_os = "linux")]
fn tree_store(tree: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(tree.join(".credentials.json")).ok()
}

/// A `--with-fallback` background launch hands its session on
/// instead of ending it. Once the launcher exits, the row of the tree a job
/// names stays, with its chain opt-in and launch account; only the launcher's
/// marker is gone, so the row waits for a daemon to hold it.
#[cfg(target_os = "linux")]
#[test]
fn a_with_fallback_background_launch_leaves_its_chain_row_for_the_daemon() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();

    let (_tree, sid) = launch_with_fallback(&fake, &config, "--bg");

    let row = crate::live_sessions::get(&sid).expect("the launcher hands its row on");
    assert_eq!(row.start_profile, "wfl");
    assert!(row.follows_chain, "the chain opt-in outlives the launcher");
    assert!(!row.isolated);
    assert!(
        row.boot_transport.is_some(),
        "the row keeps what the session booted with"
    );
    assert!(
        !crate::runtime::session_row_is_live(
            &crate::profile::ProfileName::from("wfl"),
            false,
            &sid
        ),
        "the launcher's marker died with it"
    );
}

/// A foreground `--with-fallback` session moved to the
/// background from its terminal (`/background`, or the exit dialog's "Move to
/// background and exit") leaves the same hand-off as an explicit `--bg`: the
/// launcher keys on the job that outlives it, never on its own argv.
#[cfg(target_os = "linux")]
#[test]
fn a_with_fallback_session_moved_to_the_background_is_handed_on_too() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();

    let (_tree, sid) = launch_with_fallback(&fake, &config, "/background");

    let row = crate::live_sessions::get(&sid).expect("the launcher hands its row on");
    assert!(row.follows_chain, "the chain opt-in outlives the launcher");

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the backgrounded session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
}

/// A GC pass (every clauth process runs one, and a restarted
/// daemon runs one before anything else) keeps a handed-off chain row while a
/// job names its tree, and reaps it once the job is gone. A row that never
/// opted into the chain is reaped as before, whatever a job keeps: a
/// `/background` out of a plain session only spares its tree.
#[cfg(target_os = "linux")]
#[test]
fn gc_keeps_a_handed_off_chain_row_only_while_a_job_names_its_tree() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");

    // A plain session's leftover beside it: its own tree, an empty marker
    // dir, a job naming the tree, and a row that never opted into the chain.
    let plain_sid = "777001-0";
    let profile = crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
        .expect("profile dir");
    let plain_tree = profile.join(format!("runtime-{plain_sid}"));
    std::fs::create_dir_all(&plain_tree).expect("plain tree");
    std::fs::create_dir_all(profile.join(format!("sessions-{plain_sid}")))
        .expect("plain sessions dir");
    crate::testutil::write_claude_job_in(sb.home(), "b6a1c0d2", &plain_tree, "working", None);
    let mut plain = crate::testutil::live_row(plain_sid, "wfl");
    plain.follows_chain = false;
    crate::live_sessions::register(&plain).expect("register the plain row");

    crate::runtime::gc_stale_runtimes();

    assert!(
        crate::live_sessions::get(&sid).is_some(),
        "a job keeps the handed-off chain row"
    );
    assert!(tree.is_dir(), "and its tree");
    assert!(
        crate::live_sessions::get(plain_sid).is_none(),
        "a row outside the chain is reaped whatever a job keeps"
    );
    assert!(plain_tree.is_dir(), "its tree is only spared");

    std::fs::remove_dir_all(job_naming(&sb, &tree)).expect("claude rm");
    crate::runtime::gc_stale_runtimes();

    assert!(
        crate::live_sessions::get(&sid).is_none(),
        "with no job left the row is reaped"
    );
    assert!(!tree.exists(), "and the tree collected");
}

/// A daemon that starts after the launcher exited (its boot GC
/// first) adopts the session in the tree the job names: it holds that launch's
/// own marker, re-registers the row under its own pid with what the session
/// booted with unchanged, and builds no other tree.
#[cfg(target_os = "linux")]
#[test]
fn a_daemon_adopts_a_handed_off_with_fallback_session_in_its_own_tree() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (_tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let launched = crate::live_sessions::get(&sid).expect("the launcher hands its row on");

    crate::runtime::gc_stale_runtimes();
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    let row = crate::live_sessions::get(&sid).expect("the adopted row");
    assert_eq!(row.start_profile, "wfl");
    assert_eq!(
        serde_json::to_value(&row.boot_transport).expect("serialize"),
        serde_json::to_value(&launched.boot_transport).expect("serialize"),
        "the adopted row keeps what the session booted with"
    );
    assert!(
        with_fallback_marker("wfl", &sid).is_file(),
        "the daemon holds the launch's own marker"
    );
    let profile = crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
        .expect("profile dir");
    let trees: Vec<String> = std::fs::read_dir(&profile)
        .expect("profile dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("runtime"))
        .collect();
    assert_eq!(
        trees,
        [format!("runtime-{sid}")],
        "the daemon adopted the job's tree and built no other"
    );
}

/// Claude Code saving its login over the tree's link: the link goes, a
/// regular file holding `bytes` takes its place, written after the store.
#[cfg(target_os = "linux")]
fn save_over_link(link: &std::path::Path, bytes: &[u8]) {
    std::fs::remove_file(link).expect("unlink the tree's link");
    std::fs::write(link, bytes).expect("Claude Code saves its login over the link");
    crate::testutil::set_mtime(
        link,
        std::time::SystemTime::now() + std::time::Duration::from_secs(5),
    );
}

/// `member`'s rotating store, `credentials.json`, whatever the install source.
#[cfg(target_os = "linux")]
fn rotating_store(member: &str) -> std::path::PathBuf {
    crate::profile::profile_dir(&crate::profile::ProfileName::from(member))
        .expect("profile dir")
        .join("credentials.json")
}

/// `member`'s long-lived session-token sidecar.
#[cfg(target_os = "linux")]
fn sidecar_store(member: &str) -> std::path::PathBuf {
    crate::profile::profile_dir(&crate::profile::ProfileName::from(member))
        .expect("profile dir")
        .join("session-token.json")
}

/// The bytes of `store` with one more key on its login: the shape the real
/// `claude --bg` 2.1.310 re-saves a second after launch.
#[cfg(target_os = "linux")]
fn resave_of_store(store: &std::path::Path) -> Vec<u8> {
    let mut login: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store).expect("the store"))
            .expect("the store parses");
    login["claudeAiOauth"]["subscriptionTypeSource"] = serde_json::json!("probe");
    serde_json::to_vec(&login).expect("serialize")
}

/// [`resave_of_store`] over `member`'s rotating store.
#[cfg(target_os = "linux")]
fn resave_of(member: &str) -> Vec<u8> {
    resave_of_store(&rotating_store(member))
}

/// A login Claude Code refreshed itself: tokens no store holds, and an
/// expiry later than the store's.
#[cfg(target_os = "linux")]
fn refreshed_login(access: &str) -> Vec<u8> {
    let mut creds = oauth_creds(access);
    if let Some(login) = creds.claude_ai_oauth.as_mut() {
        login.expires_at = Some(future_expiry() + 3_600_000);
    }
    serde_json::to_vec(&creds).expect("serialize")
}

/// The access token `member`'s rotating store holds.
#[cfg(target_os = "linux")]
fn stored_access(member: &str) -> String {
    let stored: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(rotating_store(member)).expect("the store"))
            .expect("the store parses");
    stored.claude_ai_oauth.expect("an OAuth login").access_token
}

/// Ticks until `daemon` holds `sid` or one adoption attempt for it failed.
#[cfg(target_os = "linux")]
fn settle_adoption(daemon: &mut Daemon, sid: &str) {
    let key = super::BackgroundKey::Session(sid.to_string());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        daemon.tick();
        if daemon.background.held.contains_key(&key) || daemon.background.retry.contains_key(&key) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no adoption attempt for {sid} settled"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The error the last failed adoption attempt of `sid` recorded.
#[cfg(target_os = "linux")]
fn adoption_error(daemon: &Daemon, sid: &str) -> Option<String> {
    daemon
        .background
        .retry
        .get(&super::BackgroundKey::Session(sid.to_string()))
        .map(|failure| failure.error.clone())
}

/// Whether `daemon`'s own map holds session `sid`.
#[cfg(target_os = "linux")]
fn daemon_maps_session(daemon: &Daemon, sid: &str) -> bool {
    daemon
        .background
        .held
        .contains_key(&super::BackgroundKey::Session(sid.to_string()))
}

#[cfg(target_os = "linux")]
fn is_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// A three-account chain (`wfl`, `wfs`, `wft`) with a session launched on
/// `wfl`, held by a first daemon and moved `wfs` -> `wft` -> back to `wfl`,
/// that daemon then gone. Returns the chain config, tree and session id.
#[cfg(target_os = "linux")]
fn session_moved_round_three_accounts(
    fake: &crate::testutil::FakeClaude<'_>,
) -> (AppConfig, std::path::PathBuf, String) {
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let mut config = persist(
        vec![
            profile_with_creds("wfl", "tok-wfl"),
            profile_with_creds("wfs", "tok-wfs"),
            profile_with_creds("wft", "tok-wft"),
        ],
        None,
        60_000,
    );
    config.state.fallback_chain = vec!["wfl".into(), "wfs".into(), "wft".into()];
    save_app_state(&config.state).expect("persist the chain");
    let (tree, sid) = launch_with_fallback(fake, &config, "--bg");
    let mut first = daemon_for(config.clone());
    tick_until(&mut first, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    for member in ["wfs", "wft", "wfl"] {
        crate::sessions_cli::run_switch(&sid, member).expect("switch along the chain");
        let store = member_store_of(member);
        tick_until(&mut first, "moved the session", |d| {
            tree_store(&tree).as_ref() == Some(&store) && daemon_holds_session(d, &sid, member)
        });
    }
    drop(first);
    (config, tree, sid)
}

/// The refusal a saved refresh no store of `wfl` holds reads as.
#[cfg(target_os = "linux")]
const NO_STORE_HOLDS_IT: &str = "the session's saved login holds a refresh token none of wfl's stores hold; refusing to adopt it";

/// The refusal an adoption reads as when a relink would leave `wfl`'s session
/// pointing at its `credentials.json` and that file is gone.
#[cfg(target_os = "linux")]
const NO_STORE_TO_RELINK: &str =
    "wfl has no credentials.json to relink the session onto; refusing to adopt it";

/// Every credential file of the three chain accounts, by path: the stores,
/// the session-token sidecars and the spent records, absent ones as `None`.
#[cfg(target_os = "linux")]
fn every_credential_file() -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    ["wfl", "wfs", "wft"]
        .iter()
        .flat_map(|member| {
            let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(*member))
                .expect("profile dir");
            ["credentials.json", "session-token.json", "auth.spent.json"].map(|name| dir.join(name))
        })
        .map(|path| {
            let bytes = std::fs::read(&path).ok();
            (path, bytes)
        })
        .collect()
}

/// Claude Code saves a login it refreshed itself over the tree's link while no
/// clauth process holds the session: tokens no store holds. Nothing proves
/// whose refresh chain that is, so the adoption refuses with its own text,
/// keeps the file byte for byte, and writes no credential file of any
/// account; the session keeps running on its own login.
#[cfg(target_os = "linux")]
fn a_refresh_no_store_holds(moved: bool) {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let (config, tree, sid) = if moved {
        session_moved_round_three_accounts(&fake)
    } else {
        let config = with_fallback_chain();
        let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
        (config, tree, sid)
    };
    let link = tree.join(".credentials.json");
    let saved = refreshed_login("tok-wfl-cc-refreshed");
    save_over_link(&link, &saved);
    let before = every_credential_file();

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(!daemon_maps_session(&daemon, &sid), "the adoption refuses");
    assert!(
        adoption_error(&daemon, &sid).is_some_and(|e| e.contains(NO_STORE_HOLDS_IT)),
        "refused as a login no store holds, got {:?}",
        adoption_error(&daemon, &sid)
    );
    assert!(!is_symlink(&link), "the tree keeps its own file");
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    assert_eq!(
        every_credential_file(),
        before,
        "no credential file is written"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_saved_refresh_no_store_holds_is_refused_and_kept() {
    a_refresh_no_store_holds(false);
}

#[cfg(target_os = "linux")]
#[test]
fn a_moved_sessions_own_refresh_is_refused_and_kept() {
    a_refresh_no_store_holds(true);
}

/// Retried adoptions of a saved refresh no store holds never ask any account
/// endpoint: no request leaves the process, whatever the attempt count.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_refresh_asks_no_endpoint() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let (deaf, base) = crate::testutil::deaf_listener();
    let _endpoints = crate::testutil::EndpointSandbox::new(&sb, &base);
    save_over_link(
        &tree.join(".credentials.json"),
        &refreshed_login("tok-wfl-cc-refreshed"),
    );
    drop(config);

    for _ in 0..3 {
        let refused = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]);
        assert!(
            refused
                .as_ref()
                .err()
                .is_some_and(|e| format!("{e:#}").contains(NO_STORE_HOLDS_IT)),
            "refused as a login no store holds"
        );
    }

    crate::testutil::assert_nothing_sent(&deaf);
}

/// The measured 2.1.310 shape on a session that never moved: the same login
/// plus one key. The daemon adopts, the tree links back to the store, and
/// the store's login is unchanged.
#[cfg(target_os = "linux")]
#[test]
fn a_daemon_adopts_a_session_whose_claude_re_saved_the_same_login() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let access = stored_access("wfl");
    save_over_link(&tree.join(".credentials.json"), &resave_of("wfl"));

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the re-saved session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    assert_eq!(tree_store(&tree), Some(member_store_of("wfl")));
    assert_eq!(stored_access("wfl"), access, "the store keeps its login");
}

/// A session that moved onto `wfs` and was held there, re-adopted by a fresh
/// daemon after Claude Code re-saved wfs's login over the link: the re-save
/// is wfs's own login, so the adoption lands on wfs and wfl is untouched;
/// the key Claude Code added inside the login stays in wfs's store.
#[cfg(target_os = "linux")]
#[test]
fn a_moved_session_whose_claude_re_saved_its_login_is_re_adopted_on_its_account() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut first = daemon_for(config.clone());
    tick_until(&mut first, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut first, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    drop(first);
    let wfl_before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    let wfs_access = stored_access("wfs");
    save_over_link(&tree.join(".credentials.json"), &resave_of("wfs"));

    let mut second = daemon_for(config);
    tick_until(&mut second, "re-adopted the session on wfs", |d| {
        daemon_holds_session(d, &sid, "wfs")
    });

    assert_eq!(tree_store(&tree), Some(wfs_store));
    assert_eq!(stored_access("wfs"), wfs_access, "wfs keeps its login");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rotating_store("wfs")).expect("wfs store"))
            .expect("the store parses");
    assert_eq!(
        stored["claudeAiOauth"]["subscriptionTypeSource"],
        serde_json::json!("probe"),
        "the key Claude Code added inside its login stays"
    );
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        wfl_before,
        "wfl is never written"
    );
}

/// Claude Code re-saved the store's login, then clauth rotated the store
/// (the re-saved refresh token is now recorded spent): the file is the
/// store's own older login, so the adoption lands on the rotated store and
/// keeps it.
#[cfg(target_os = "linux")]
fn rotated_after_re_save(moved: bool) {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let (config, tree, sid) = if moved {
        session_moved_round_three_accounts(&fake)
    } else {
        let config = with_fallback_chain();
        let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
        (config, tree, sid)
    };
    let link = tree.join(".credentials.json");
    save_over_link(&link, &resave_of("wfl"));
    let old: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(rotating_store("wfl")).expect("the store"))
            .expect("the store parses");
    let old_refresh = old
        .claude_ai_oauth
        .and_then(|login| login.refresh_token)
        .expect("a refresh token");
    std::fs::write(
        rotating_store("wfl"),
        serde_json::to_vec(&oauth_creds("tok-wfl-rotated")).expect("serialize"),
    )
    .expect("the rotation lands");
    std::fs::write(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
            .expect("profile dir")
            .join("auth.spent.json"),
        serde_json::to_vec(&vec![crate::codex_auth::token_fingerprint(&old_refresh)])
            .expect("serialize"),
    )
    .expect("the rotation records the spent token");

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the session on the rotated store", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    assert_eq!(
        stored_access("wfl"),
        "tok-wfl-rotated",
        "the store keeps the rotated login"
    );
    assert!(
        is_symlink(&link),
        "and the tree reads the rotated store again"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_older_re_saved_login_of_a_session_that_never_moved_lands_on_the_rotated_store() {
    rotated_after_re_save(false);
}

#[cfg(target_os = "linux")]
#[test]
fn an_older_re_saved_login_of_a_moved_session_lands_on_the_rotated_store() {
    rotated_after_re_save(true);
}

/// A swap recorded `wfs` as visited and moved the link onto it, but its write
/// of the row's member never landed, so the row still names `wfl`; Claude
/// Code then saves a refreshed wfs login over the link. No store of wfl holds
/// it, so the adoption refuses and wfl's store is untouched.
#[cfg(target_os = "linux")]
#[test]
fn another_accounts_saved_refresh_never_lands_in_the_rows_member() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    crate::live_sessions::update_as_session(&sid, |f| f.record_visited_member("wfs"))
        .expect("the swap records the visit first");
    save_over_link(
        &tree.join(".credentials.json"),
        &refreshed_login("tok-wfs-refreshed"),
    );
    let wfl_before = std::fs::read(rotating_store("wfl")).expect("wfl store");

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(!daemon_maps_session(&daemon, &sid), "the adoption refuses");
    assert!(
        adoption_error(&daemon, &sid).is_some_and(|e| e.contains(NO_STORE_HOLDS_IT)),
        "refused as a login no store of wfl holds, got {:?}",
        adoption_error(&daemon, &sid)
    );
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        wfl_before,
        "wfl's store never takes a login the row cannot attribute to wfl"
    );
}

/// The same lost-write state, with Claude Code re-saving wfs's current login
/// (the 2.1.310 shape) over the link: still another account's login.
#[cfg(target_os = "linux")]
#[test]
fn another_accounts_re_saved_login_never_lands_in_the_rows_member() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    crate::live_sessions::update_as_session(&sid, |f| f.record_visited_member("wfs"))
        .expect("the swap records the visit first");
    save_over_link(&tree.join(".credentials.json"), &resave_of("wfs"));
    let wfl_before = std::fs::read(rotating_store("wfl")).expect("wfl store");

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(!daemon_maps_session(&daemon, &sid), "the adoption refuses");
    assert!(
        adoption_error(&daemon, &sid).is_some_and(|e| e.contains(NO_STORE_HOLDS_IT)),
        "refused as a login no store of wfl holds, got {:?}",
        adoption_error(&daemon, &sid)
    );
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        wfl_before,
        "wfl's store never takes wfs's login"
    );
}

/// Launched on a long-lived session token; Claude Code re-saves that token's
/// login over the link; the user clears the token before any daemon holds the
/// session. The static token never lands over the rotating pair.
#[cfg(target_os = "linux")]
#[test]
fn a_cleared_session_token_never_lands_over_the_rotating_login() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let wfl = crate::profile::ProfileName::from("wfl");
    crate::testutil::write_long_lived_sidecar(&wfl);
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    assert_eq!(
        tree_store(&tree),
        std::fs::canonicalize(sidecar_store("wfl")).ok(),
        "launched onto the session token"
    );
    save_over_link(
        &tree.join(".credentials.json"),
        &resave_of_store(&sidecar_store("wfl")),
    );
    assert!(
        crate::claude::clear_session_token(&wfl).expect("clear"),
        "token removed"
    );
    let creds_before = std::fs::read(rotating_store("wfl")).expect("credentials.json");

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("credentials.json"),
        creds_before,
        "the static token never lands over the rotating pair"
    );
}

/// A session that never moved; a converge onto a long-lived session token
/// relinked its tree but the row's `launch_store` write never landed (it
/// still names `credentials.json`); Claude Code then re-saves the token's
/// login over the link. The static token never lands over the rotating pair.
#[cfg(target_os = "linux")]
#[test]
fn a_stale_launch_store_never_lands_the_session_token_over_the_rotating_login() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let wfl = crate::profile::ProfileName::from("wfl");
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let link = tree.join(".credentials.json");
    crate::testutil::write_long_lived_sidecar(&wfl);
    crate::claude::publish_credential_link(&link, &sidecar_store("wfl"))
        .expect("the converge relinks the tree");
    save_over_link(&link, &resave_of_store(&sidecar_store("wfl")));
    let creds_before = std::fs::read(rotating_store("wfl")).expect("credentials.json");

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("credentials.json"),
        creds_before,
        "the static token never lands over the rotating pair"
    );
}

/// A refused adoption (another holder keeps a marker of an account the session
/// ran on) over a tree whose credential file Claude Code re-saved: the tree's
/// file and every store stay exactly as they were.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_adoption_never_drains_the_trees_saved_login() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let (config, tree, sid) = session_moved_round_three_accounts(&fake);
    let foreign = with_fallback_marker("wft", &sid);
    std::fs::create_dir_all(foreign.parent().expect("marker dir")).expect("the holder's dir");
    let held = crate::runtime::open_pid_file(&foreign).expect("foreign marker");
    held.lock().expect("another holder keeps wft's marker");
    let link = tree.join(".credentials.json");
    let saved = refreshed_login("tok-wfl-refreshed");
    save_over_link(&link, &saved);
    let wfl_before = std::fs::read(rotating_store("wfl")).expect("wfl store");

    let mut second = daemon_for(config);
    settle_adoption(&mut second, &sid);

    assert!(
        !daemon_maps_session(&second, &sid),
        "refused on the foreign marker"
    );
    assert!(
        !is_symlink(&link),
        "a refused adoption leaves the held tree's file in place"
    );
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        wfl_before,
        "and writes no store"
    );
    drop(held);
}

/// The same refusal over a pure re-save, which the login checks accept: only
/// the order (marker claims first) keeps the refused adoption from relinking
/// a tree another holder keeps.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_adoption_of_a_re_saved_login_keeps_the_trees_file() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let (config, tree, sid) = session_moved_round_three_accounts(&fake);
    let foreign = with_fallback_marker("wft", &sid);
    std::fs::create_dir_all(foreign.parent().expect("marker dir")).expect("the holder's dir");
    let held = crate::runtime::open_pid_file(&foreign).expect("foreign marker");
    held.lock().expect("another holder keeps wft's marker");
    let link = tree.join(".credentials.json");
    let saved = resave_of("wfl");
    save_over_link(&link, &saved);

    let mut second = daemon_for(config);
    settle_adoption(&mut second, &sid);

    assert!(!daemon_maps_session(&second, &sid), "refused");
    assert!(!is_symlink(&link), "a refused adoption leaves the file");
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    drop(held);
}

/// A session that never moved, its own marker held by another process: the
/// adoption called directly (the daemon's queue skips a held marker before it
/// gets this far) refuses on the claim and leaves the re-saved file and the
/// store alone.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_adoption_of_a_session_that_never_moved_drains_nothing() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let marker = with_fallback_marker("wfl", &sid);
    std::fs::create_dir_all(marker.parent().expect("marker dir")).expect("the holder's dir");
    let held = crate::runtime::open_pid_file(&marker).expect("the session's marker");
    held.lock()
        .expect("another holder keeps the session's own marker");
    let link = tree.join(".credentials.json");
    let saved = resave_of("wfl");
    save_over_link(&link, &saved);
    let wfl_before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    drop(config);

    let refused = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]);

    assert!(refused.is_err(), "refused on the held marker");
    assert!(!is_symlink(&link), "the tree keeps its own file");
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        wfl_before,
        "and writes no store"
    );
    drop(held);
}

/// A spent-token record beside the store that cannot be read: the saved login
/// cannot be judged, so the adoption refuses on that, names it, and leaves
/// both copies, never relinking the session away from what may be the
/// account's only live refresh token.
#[cfg(target_os = "linux")]
#[test]
fn an_unjudged_saved_login_keeps_both_copies_and_refuses() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    std::fs::write(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
            .expect("profile dir")
            .join("auth.spent.json"),
        b"{ torn",
    )
    .expect("a torn spent record");
    let link = tree.join(".credentials.json");
    let saved = refreshed_login("tok-wfl-refreshed");
    save_over_link(&link, &saved);
    let wfl_before = std::fs::read(rotating_store("wfl")).expect("wfl store");

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(
        !daemon_maps_session(&daemon, &sid),
        "no adoption over an unjudged login"
    );
    assert!(
        adoption_error(&daemon, &sid).is_some_and(|e| e.contains("cannot be judged")),
        "refused by the spent record, not by a later check"
    );
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        wfl_before,
        "the store is untouched"
    );
}

/// The session launched on wfl's rotating store; a long-lived session token is
/// armed before any daemon adopts, so today's install source is the sidecar.
/// Claude Code saved a refresh of the rotating login it read: no store holds
/// it, so neither the store the session launched on nor the sidecar is
/// written.
#[cfg(target_os = "linux")]
#[test]
fn a_saved_refresh_writes_neither_store_when_a_session_token_is_armed() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    assert_eq!(
        tree_store(&tree),
        std::fs::canonicalize(rotating_store("wfl")).ok(),
        "launched onto the rotating store"
    );
    crate::testutil::write_long_lived_sidecar(&crate::profile::ProfileName::from("wfl"));
    save_over_link(
        &tree.join(".credentials.json"),
        &refreshed_login("tok-wfl-refreshed"),
    );
    let before = every_credential_file();

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(!daemon_maps_session(&daemon, &sid), "the adoption refuses");
    assert_eq!(
        every_credential_file(),
        before,
        "no credential file is written"
    );
}

/// Claude Code's dead-token wipe (a blank pair, other keys kept) saved over the
/// link of a session that never moved: no login to protect, so the adoption
/// holds the session and relinks it onto the store. Peer sessions on the same
/// store kept writing through their links meanwhile, so the store's MCP logins
/// are the fresher ones: the shell's older copy never lands over them, and
/// the store is not written at all.
#[cfg(target_os = "linux")]
#[test]
fn a_logged_out_shell_saved_over_the_link_is_relinked_to_the_store() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let link = tree.join(".credentials.json");
    save_over_link(
        &link,
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":{"srv":{"accessToken":"shell-older","expiresAt":9999999999000}}}"#,
    );
    let mut store = stored_json("wfl");
    store["mcpOAuth"] = serde_json::json!({
        "srv": {"accessToken": "peer-fresher", "expiresAt": 9_999_999_999_999_i64},
        "peer-only": {"accessToken": "p", "expiresAt": 9_999_999_999_999_i64},
    });
    std::fs::write(
        rotating_store("wfl"),
        serde_json::to_vec(&store).expect("serialize"),
    )
    .expect("a peer session's MCP logins land in the store");
    let before = std::fs::read(rotating_store("wfl")).expect("wfl store");

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the logged-out session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    assert!(is_symlink(&link), "the shell is relinked onto the store");
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        before,
        "the store, its peers' MCP logins included, is not written"
    );
}

/// A logged-out shell holding MCP logins the store lacks or holds older copies
/// of: they merge into the store per server, the later expiry winning, while a
/// server only the store holds stays, the store's login is untouched, and the
/// shell's design login (account-scoped, so never carried) stays out.
#[cfg(target_os = "linux")]
#[test]
fn a_logged_out_shells_mcp_logins_merge_into_the_store_fresher_wins() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let link = tree.join(".credentials.json");
    save_over_link(
        &link,
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":{"srv":{"accessToken":"shell-fresher","expiresAt":9999999999999},"stale":{"accessToken":"shell-older","expiresAt":9999999990000},"solo":{"accessToken":"shell-only","expiresAt":9999999999999}},"designOauth":{"accessToken":"shell-design","expiresAt":9999999999999}}"#,
    );
    let mut store = stored_json("wfl");
    store["mcpOAuth"] = serde_json::json!({
        "srv": {"accessToken": "store-older", "expiresAt": 9_999_999_990_000_i64},
        "stale": {"accessToken": "store-fresher", "expiresAt": 9_999_999_999_999_i64},
        "peer-only": {"accessToken": "p", "expiresAt": 9_999_999_999_999_i64},
    });
    std::fs::write(
        rotating_store("wfl"),
        serde_json::to_vec(&store).expect("serialize"),
    )
    .expect("a peer session's MCP logins land in the store");
    let login_before = store["claudeAiOauth"].clone();

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the logged-out session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    assert!(is_symlink(&link), "the shell is relinked onto the store");
    let after = stored_json("wfl");
    assert_eq!(
        after["mcpOAuth"],
        serde_json::json!({
            "srv": {"accessToken": "shell-fresher", "expiresAt": 9_999_999_999_999_i64},
            "stale": {"accessToken": "store-fresher", "expiresAt": 9_999_999_999_999_i64},
            "peer-only": {"accessToken": "p", "expiresAt": 9_999_999_999_999_i64},
            "solo": {"accessToken": "shell-only", "expiresAt": 9_999_999_999_999_i64},
        }),
        "per server, the later expiry wins and every server either side holds stays"
    );
    assert_eq!(
        after["claudeAiOauth"], login_before,
        "the store's login is untouched"
    );
    assert!(
        after.get("designOauth").is_none(),
        "the shell's account-scoped design login is never carried"
    );
}

/// `member`'s rotating store with `mcp` as its MCP logins.
#[cfg(target_os = "linux")]
fn set_store_mcp(member: &str, mcp: serde_json::Value) {
    let mut store = stored_json(member);
    store["mcpOAuth"] = mcp;
    std::fs::write(
        rotating_store(member),
        serde_json::to_vec(&store).expect("serialize"),
    )
    .expect("the store's MCP logins");
}

/// A logged-out shell whose MCP block is not a map (null) carries no server,
/// so every server the store holds stays.
#[cfg(target_os = "linux")]
#[test]
fn a_shell_with_a_null_mcp_block_never_drops_the_stores_servers() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    save_over_link(
        &tree.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":null}"#,
    );
    set_store_mcp(
        "wfl",
        serde_json::json!({"peer": {"accessToken": "p", "expiresAt": 9_999_999_999_999_i64}}),
    );
    let before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    drop(config);

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        before,
        "the store keeps every server, and nothing is written"
    );
    drop(held);
}

/// Neither copy of a server's MCP login carries an expiry, so which is fresher
/// cannot be told: the store's copy, the one peer sessions write, stays.
#[cfg(target_os = "linux")]
#[test]
fn an_expiryless_shell_copy_never_replaces_the_stores() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    save_over_link(
        &tree.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":{"srv":{"accessToken":"shell-copy","refreshToken":"mrt-shell"},"tie":{"accessToken":"shell-tie","expiresAt":9999999999999}}}"#,
    );
    set_store_mcp(
        "wfl",
        serde_json::json!({
            "srv": {"accessToken": "store-copy", "refreshToken": "mrt-store"},
            "tie": {"accessToken": "store-tie", "expiresAt": 9_999_999_999_999_i64},
        }),
    );
    let before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    drop(config);

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        before,
        "an expiryless or tied shell copy never lands over the store's"
    );
    drop(held);
}

/// A legacy `credentials.json.pending` rotation (written by a clauth that
/// predates the staged form, newer than the store) sits beside wfl's store
/// when Claude Code's re-save of the store's login is adopted: the adoption
/// never writes the store over it, so a read still resolves the pending login
/// and its refresh token is the one that gets used next.
#[cfg(target_os = "linux")]
#[test]
fn a_re_save_never_buries_a_legacy_pending_rotation() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &resave_of("wfl"));
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(
        &rotating_store("wfl"),
        now - std::time::Duration::from_secs(120),
    );
    let wfl = crate::profile::ProfileName::from("wfl");
    crate::profile::stage_rotated_credentials(&wfl, &oauth_creds("tok-wfl-pending"))
        .expect("a legacy pending rotation");
    crate::testutil::set_mtime(
        &rotating_store("wfl").with_file_name("credentials.json.pending"),
        now - std::time::Duration::from_secs(60),
    );
    let resolved = |what: &str| {
        crate::profile::read_profile(&wfl)
            .expect(what)
            .credentials
            .and_then(|creds| creds.claude_ai_oauth)
            .map(|login| login.access_token)
    };
    assert_eq!(
        resolved("read before").as_deref(),
        Some("tok-wfl-pending"),
        "the fixture's pending rotation resolves before the adoption"
    );
    drop(config);

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    assert_eq!(
        resolved("read after").as_deref(),
        Some("tok-wfl-pending"),
        "the pending rotation still resolves after the adoption"
    );
    assert!(is_symlink(&link), "the re-save is relinked onto the store");
    drop(held);
}

/// A re-save of wfl's login that also carries an MCP login, adopted while a
/// legacy pending rotation sits beside wfl's store: nothing is written to the
/// store, and the MCP login that leaves with the relink is reported.
#[cfg(target_os = "linux")]
#[test]
fn a_re_save_beside_a_legacy_pending_rotation_reports_the_keys_it_drops() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&resave_of("wfl")).expect("the re-save parses");
    saved["mcpOAuth"] =
        serde_json::json!({"srv": {"accessToken": "m", "expiresAt": 9_999_999_999_999_i64}});
    save_over_link(
        &tree.join(".credentials.json"),
        &serde_json::to_vec(&saved).expect("serialize"),
    );
    crate::profile::stage_rotated_credentials(
        &crate::profile::ProfileName::from("wfl"),
        &oauth_creds("tok-wfl-pending"),
    )
    .expect("a legacy pending rotation");
    crate::testutil::set_mtime(
        &rotating_store("wfl").with_file_name("credentials.json.pending"),
        std::time::SystemTime::now() + std::time::Duration::from_secs(30),
    );
    let before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    drop(config);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    drop(capture);
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        before,
        "the store is not written beside the pending rotation"
    );
    let captured = lines.snapshot();
    assert!(
        captured
            .iter()
            .any(|line| line.contains("a rotation is staged beside")),
        "the dropped MCP login is reported, got {captured:?}"
    );
    drop(held);
}

/// The same legacy pending rotation beside wfl's store when a logged-out
/// shell carrying an MCP login is adopted: the merge stands down rather than
/// write the store over the pending pair, and its event line never promises a
/// retry, since the relink that follows takes the shell's file away.
#[cfg(target_os = "linux")]
#[test]
fn a_shell_merge_stands_down_beside_a_legacy_pending_rotation() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    save_over_link(
        &tree.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":{"solo":{"accessToken":"shell-only","expiresAt":9999999999999}}}"#,
    );
    crate::profile::stage_rotated_credentials(
        &crate::profile::ProfileName::from("wfl"),
        &oauth_creds("tok-wfl-pending"),
    )
    .expect("a legacy pending rotation");
    crate::testutil::set_mtime(
        &rotating_store("wfl").with_file_name("credentials.json.pending"),
        std::time::SystemTime::now() + std::time::Duration::from_secs(30),
    );
    let before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    drop(config);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    drop(capture);
    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        before,
        "the store is not written beside the pending rotation"
    );
    let captured = lines.snapshot();
    assert!(
        captured
            .iter()
            .any(|line| line.contains("a rotation is staged beside")),
        "the stand-down is reported, got {captured:?}"
    );
    assert!(
        !captured.iter().any(|line| line.contains("retries")),
        "and promises no retry, got {captured:?}"
    );
    drop(held);
}

/// With `preserve_non_login_keys` off and a long-lived session token as the
/// install source, a logged-out shell is relinked onto the sidecar and its MCP
/// logins never land there: every sidecar writer rebuilds that file from its
/// token alone.
#[cfg(target_os = "linux")]
#[test]
fn a_shell_merge_never_writes_a_sidecar_with_preservation_off() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    crate::testutil::write_long_lived_sidecar(&crate::profile::ProfileName::from("wfl"));
    let mut state = crate::profile::load_app_state().expect("state");
    state.preserve_non_login_keys = false;
    save_app_state(&state).expect("preservation off");
    save_over_link(
        &tree.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":{"solo":{"accessToken":"shell-only","expiresAt":9999999999999}}}"#,
    );
    let before = every_credential_file();
    drop(config);

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    assert_eq!(
        tree_store(&tree),
        std::fs::canonicalize(sidecar_store("wfl")).ok(),
        "relinked onto the session token"
    );
    assert_eq!(
        every_credential_file(),
        before,
        "no credential file is written"
    );
    drop(held);
}

/// A logged-out shell whose MCP block is null over a store that holds no MCP
/// logins at all: nothing is carried, so the store never gains a null block.
#[cfg(target_os = "linux")]
#[test]
fn a_null_mcp_block_never_lands_in_a_store_without_one() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    save_over_link(
        &tree.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":null}"#,
    );
    assert!(
        stored_json("wfl").get("mcpOAuth").is_none(),
        "the fixture's store holds no MCP logins"
    );
    let before = std::fs::read(rotating_store("wfl")).expect("wfl store");
    drop(config);

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("wfl store"),
        before,
        "the store gains no null MCP block"
    );
    drop(held);
}

/// An older login of `wfl` (its refresh token recorded spent) carrying an MCP
/// login is saved over the link while a legacy pending rotation sits beside
/// the store: the carry stands down and says so, and since the relink right
/// after takes the session's file away, its line never promises a retry.
#[cfg(target_os = "linux")]
#[test]
fn an_older_logins_carry_beside_a_pending_rotation_promises_no_retry() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut saved = stored_json("wfl");
    saved["mcpOAuth"] =
        serde_json::json!({"srv": {"accessToken": "m", "expiresAt": 9_999_999_999_999_i64}});
    let old_refresh = saved["claudeAiOauth"]["refreshToken"]
        .as_str()
        .expect("a refresh token")
        .to_string();
    std::fs::write(
        rotating_store("wfl"),
        serde_json::to_vec(&oauth_creds("tok-wfl-rotated")).expect("serialize"),
    )
    .expect("the rotation lands");
    std::fs::write(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
            .expect("profile dir")
            .join("auth.spent.json"),
        serde_json::to_vec(&vec![crate::codex_auth::token_fingerprint(&old_refresh)])
            .expect("serialize"),
    )
    .expect("the rotation records the spent token");
    crate::profile::stage_rotated_credentials(
        &crate::profile::ProfileName::from("wfl"),
        &oauth_creds("tok-wfl-pending"),
    )
    .expect("a legacy pending rotation");
    crate::testutil::set_mtime(
        &rotating_store("wfl").with_file_name("credentials.json.pending"),
        std::time::SystemTime::now() + std::time::Duration::from_secs(30),
    );
    save_over_link(
        &tree.join(".credentials.json"),
        &serde_json::to_vec(&saved).expect("serialize"),
    );
    drop(config);
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    drop(capture);
    let captured = lines.snapshot();
    assert!(
        captured
            .iter()
            .any(|line| line.contains("a rotation is staged beside")),
        "the stand-down is reported, got {captured:?}"
    );
    assert!(
        !captured.iter().any(|line| line.contains("retries")),
        "and promises no retry, got {captured:?}"
    );
    drop(held);
}

/// The bytes of `member`'s rotating store, parsed.
#[cfg(target_os = "linux")]
fn stored_json(member: &str) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(rotating_store(member)).expect("the store"))
        .expect("the store parses")
}

/// An older login of `wfl` (its refresh token recorded spent) saved over the
/// link with an MCP login the session made: the adoption relinks onto the
/// store and the MCP login rides into it.
#[cfg(target_os = "linux")]
#[test]
fn an_older_logins_mcp_login_survives_the_relink() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut saved = stored_json("wfl");
    saved["mcpOAuth"] =
        serde_json::json!({"srv": {"accessToken": "m", "expiresAt": 9_999_999_999_999_i64}});
    let old_refresh = saved["claudeAiOauth"]["refreshToken"]
        .as_str()
        .expect("a refresh token")
        .to_string();
    std::fs::write(
        rotating_store("wfl"),
        serde_json::to_vec(&oauth_creds("tok-wfl-rotated")).expect("serialize"),
    )
    .expect("the rotation lands");
    std::fs::write(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
            .expect("profile dir")
            .join("auth.spent.json"),
        serde_json::to_vec(&vec![crate::codex_auth::token_fingerprint(&old_refresh)])
            .expect("serialize"),
    )
    .expect("the rotation records the spent token");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &serde_json::to_vec(&saved).expect("serialize"));

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the session on the rotated store", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    assert!(is_symlink(&link), "relinked onto the rotated store");
    let after = stored_json("wfl");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"],
        serde_json::json!("tok-wfl-rotated"),
        "the store keeps its rotated login"
    );
    assert_eq!(
        after["mcpOAuth"]["srv"]["accessToken"],
        serde_json::json!("m"),
        "the older login's MCP login survives the relink"
    );
}

/// The store's current login re-saved with a blank access token: nothing to
/// write, so the adoption holds the session and relinks it onto the store,
/// whose login stays as it was.
#[cfg(target_os = "linux")]
#[test]
fn a_re_save_with_a_blank_access_token_is_relinked_onto_the_store() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let login_before = stored_json("wfl")["claudeAiOauth"].clone();
    let mut saved = stored_json("wfl");
    saved["claudeAiOauth"]["accessToken"] = serde_json::json!("");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &serde_json::to_vec(&saved).expect("serialize"));

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    assert!(is_symlink(&link), "relinked onto the store");
    assert_eq!(
        stored_json("wfl")["claudeAiOauth"],
        login_before,
        "the store's login is untouched"
    );
}

/// An older login of `wfl` saved over the link after wfl's store is gone:
/// there is nothing to relink onto, so the adoption refuses with its own
/// text before it touches the session's file.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_adoption_of_an_older_login_keeps_the_file_when_the_store_is_gone() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let saved = resave_of("wfl");
    let old_refresh = stored_json("wfl")["claudeAiOauth"]["refreshToken"]
        .as_str()
        .expect("a refresh token")
        .to_string();
    std::fs::write(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
            .expect("profile dir")
            .join("auth.spent.json"),
        serde_json::to_vec(&vec![crate::codex_auth::token_fingerprint(&old_refresh)])
            .expect("serialize"),
    )
    .expect("the record");
    std::fs::remove_file(rotating_store("wfl")).expect("the store goes");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &saved);
    drop(config);

    let refused = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]);

    assert!(
        refused
            .as_ref()
            .err()
            .is_some_and(|e| format!("{e:#}").contains(NO_STORE_TO_RELINK)),
        "refused as having no store to relink onto, got {:?}",
        refused.as_ref().err().map(|e| format!("{e:#}"))
    );
    assert_eq!(
        std::fs::read(&link).expect("the tree's file is kept"),
        saved
    );
}

/// A static token saved over the link of a member with no store left: the
/// adoption refuses with its own text before it touches the session's file.
#[cfg(target_os = "linux")]
#[test]
fn a_static_token_with_no_store_to_relink_onto_keeps_the_file() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    std::fs::remove_file(rotating_store("wfl")).expect("the store goes");
    let saved = serde_json::to_vec(&serde_json::json!({
        "claudeAiOauth": {"accessToken": "tok-static", "expiresAt": future_expiry()}
    }))
    .expect("serialize");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &saved);
    drop(config);

    let refused = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]);

    assert!(
        refused
            .as_ref()
            .err()
            .is_some_and(|e| format!("{e:#}").contains(NO_STORE_TO_RELINK)),
        "refused as having no store to relink onto, got {:?}",
        refused.as_ref().err().map(|e| format!("{e:#}"))
    );
    assert_eq!(
        std::fs::read(&link).expect("the tree's file is kept"),
        saved
    );
}

/// A rotation staged beside the store (its new login written, the old token
/// recorded spent, the store not yet replaced) when Claude Code re-saves the
/// store's pre-rotation login: the stage lands first, so the re-save is
/// judged as the older login it now is and the store keeps the staged login.
#[cfg(target_os = "linux")]
#[test]
fn a_live_stage_lands_before_the_adoption_judges_the_store() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let wfl_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
        .expect("profile dir");
    let base_refresh = stored_json("wfl")["claudeAiOauth"]["refreshToken"]
        .as_str()
        .expect("a refresh token")
        .to_string();
    let base = crate::codex_auth::token_fingerprint(&base_refresh);
    let stage = serde_json::json!({
        "base": base,
        "base_access": stored_access("wfl"),
        "creds": {"claudeAiOauth": {
            "accessToken": "tok-staged",
            "refreshToken": "rt-tok-staged",
            "expiresAt": future_expiry() + 7_200_000,
        }},
    });
    std::fs::write(
        wfl_dir.join("credentials.json.staged"),
        serde_json::to_vec(&stage).expect("serialize"),
    )
    .expect("the stage");
    std::fs::write(
        wfl_dir.join("auth.spent.json"),
        serde_json::to_vec(&vec![base]).expect("serialize"),
    )
    .expect("the spent record");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &resave_of("wfl"));
    drop(config);

    let held = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]).expect("adopted");

    assert_eq!(
        stored_access("wfl"),
        "tok-staged",
        "the stage lands before the rules judge the store"
    );
    assert!(
        is_symlink(&link),
        "the older re-save is relinked onto the store"
    );
    drop(held);
}

/// A session token saved with a blank refresh token is still a static token:
/// it never lands over the rotating pair.
#[cfg(target_os = "linux")]
#[test]
fn a_session_token_with_a_blank_refresh_token_never_lands_over_the_rotating_login() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let wfl = crate::profile::ProfileName::from("wfl");
    crate::testutil::write_long_lived_sidecar(&wfl);
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let token = crate::claude::installed_session_token(&wfl).expect("the sidecar's token");
    let saved = serde_json::to_vec(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": token,
            "refreshToken": "",
            "expiresAt": future_expiry() + 7_200_000,
        }
    }))
    .expect("serialize");
    save_over_link(&tree.join(".credentials.json"), &saved);
    let creds_before = std::fs::read(rotating_store("wfl")).expect("credentials.json");

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert_eq!(
        std::fs::read(rotating_store("wfl")).expect("credentials.json"),
        creds_before,
        "the static token never lands in credentials.json"
    );
}

/// The session re-saved the store's login, then the store moved on to a later
/// login through a path that recorded nothing spent: no record says the
/// file's login is an older one, so the adoption refuses, keeps the file and
/// leaves the newer store as it is.
#[cfg(target_os = "linux")]
#[test]
fn a_login_the_store_moved_past_unrecorded_is_refused_and_kept() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let link = tree.join(".credentials.json");
    let saved = resave_of("wfl");
    save_over_link(&link, &saved);
    std::fs::write(rotating_store("wfl"), refreshed_login("tok-wfl-peer"))
        .expect("a peer's later login lands in the store");
    let before = every_credential_file();

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(!daemon_maps_session(&daemon, &sid), "the adoption refuses");
    assert!(
        adoption_error(&daemon, &sid).is_some_and(|e| e.contains(NO_STORE_HOLDS_IT)),
        "refused as a login no store holds, got {:?}",
        adoption_error(&daemon, &sid)
    );
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    assert_eq!(
        every_credential_file(),
        before,
        "no credential file is written"
    );
}

/// A re-save from before the store gained a key outside the login (a peer
/// session's MCP login): the adoption keeps that key in the store.
#[cfg(target_os = "linux")]
#[test]
fn a_re_save_keeps_store_keys_added_since() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    save_over_link(&tree.join(".credentials.json"), &resave_of("wfl"));
    let mut store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rotating_store("wfl")).expect("the store"))
            .expect("the store parses");
    store["mcpOAuth"] = serde_json::json!({"peer": {"accessToken": "peer-mcp"}});
    std::fs::write(
        rotating_store("wfl"),
        serde_json::to_vec(&store).expect("serialize"),
    )
    .expect("a peer's MCP login lands in the store");
    crate::testutil::set_mtime(
        &rotating_store("wfl"),
        std::time::SystemTime::now() + std::time::Duration::from_secs(10),
    );

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the re-saved session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rotating_store("wfl")).expect("the store"))
            .expect("the store parses");
    assert_eq!(
        after["mcpOAuth"]["peer"]["accessToken"],
        serde_json::json!("peer-mcp"),
        "the store keeps a key the re-save predates"
    );
}

/// Two clauth profiles on one account, neither recording which account it is:
/// the session last ran on `wfs`, whose store rotated, and Claude Code re-saved
/// wfs's older login. Nothing of wfl's holds that login, so it never lands in
/// wfl's store, where the two stores would share one single-use refresh chain.
#[cfg(target_os = "linux")]
#[test]
fn another_profiles_login_on_the_same_account_never_lands() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    crate::live_sessions::update_as_session(&sid, |f| f.record_visited_member("wfs"))
        .expect("the swap records the visit first");
    let saved = resave_of("wfs");
    std::fs::write(rotating_store("wfs"), refreshed_login("tok-wfs-rotated")).expect("wfs rotates");
    let link = tree.join(".credentials.json");
    save_over_link(&link, &saved);
    let before = every_credential_file();

    let mut daemon = daemon_for(config);
    settle_adoption(&mut daemon, &sid);

    assert!(!daemon_maps_session(&daemon, &sid), "the adoption refuses");
    assert!(
        adoption_error(&daemon, &sid).is_some_and(|e| e.contains(NO_STORE_HOLDS_IT)),
        "refused as a login no store of wfl holds, got {:?}",
        adoption_error(&daemon, &sid)
    );
    assert_eq!(std::fs::read(&link).expect("the tree's file"), saved);
    assert_eq!(
        every_credential_file(),
        before,
        "wfl's store never takes wfs's chain"
    );
}

/// An adopted session follows the chain. A manual
/// `clauth switch <sid> wfs` is taken (only the fixed `runtime-bg` session is
/// fixed-account), and the daemon's own watchdog for the session moves the
/// tree's credential link onto `wfs`, records the move on the row and holds a
/// marker under `wfs`, the account the session now spends.
#[cfg(target_os = "linux")]
#[test]
fn an_adopted_with_fallback_session_moves_along_the_chain() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    crate::sessions_cli::run_switch(&sid, "wfs").expect("an adopted chain session is switchable");

    let wfs_store = member_store_of("wfs");
    tick_until(&mut daemon, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store)
            && crate::live_sessions::get(&sid)
                .and_then(|row| row.current_member)
                .as_deref()
                == Some("wfs")
            && daemon_holds_session(d, &sid, "wfs")
    });
    assert!(
        crate::runtime::has_live_marker(&crate::profile::ProfileName::from("wfs")),
        "the account the session spends now has a live marker"
    );
}

/// A launcher that already moved its session before it exited
/// leaves the tree on that member. The adoption holds the member the session
/// runs on (its marker under that account, so the account cannot be deleted
/// or renamed under it) and moves it back on intent.
#[cfg(target_os = "linux")]
#[test]
fn an_adoption_holds_the_member_the_session_last_moved_onto() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    // The launcher's own swap onto `wfs` before it exited: the tree's link on
    // `wfs`'s store, the row's current member `wfs`.
    let link = tree.join(".credentials.json");
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(
        crate::claude::install_source_path(&crate::profile::ProfileName::from("wfs"))
            .expect("install source"),
        &link,
    )
    .expect("link onto wfs");
    crate::live_sessions::update_as_session(&sid, |fields| fields.set_current_member("wfs"))
        .expect("record the launcher's move");

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the session on wfs", |d| {
        daemon_holds_session(d, &sid, "wfs")
    });
    assert!(
        with_fallback_marker("wfs", &sid).is_file(),
        "the daemon holds the session's marker under wfs"
    );

    crate::live_sessions::update_as_daemon(&sid, |fields| fields.set_intended_member("wfl"))
        .expect("intent back onto wfl");
    let wfl_store = member_store_of("wfl");
    tick_until(&mut daemon, "moved the session back onto wfl", |_| {
        tree_store(&tree).as_ref() == Some(&wfl_store)
            && crate::live_sessions::get(&sid)
                .and_then(|row| row.current_member)
                .as_deref()
                == Some("wfl")
    });
}

/// The tick never waits on the locks an adoption takes. With
/// the launch account's rotation lock and the state flock held elsewhere, a
/// tick returns at once and the adoption lands once they free.
#[cfg(target_os = "linux")]
#[test]
fn a_tick_never_waits_on_the_locks_a_with_fallback_adoption_takes() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (_tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config);

    let (rot_held_tx, rot_held_rx) = std::sync::mpsc::channel();
    let (rot_release_tx, rot_release_rx) = std::sync::mpsc::channel::<()>();
    let rotation_holder = std::thread::spawn(move || {
        let rotation = crate::testutil::hold_rotation_lock("wfl");
        rot_held_tx.send(()).expect("report the rotation hold");
        // Released after the tick's 5 s budget even if nothing signals, so a
        // tick that waits on it returns and the elapsed assertion fails.
        let _ = rot_release_rx.recv_timeout(Duration::from_secs(6));
        drop(rotation);
    });
    rot_held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the rotation lock is held");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let flock_holder = std::thread::spawn(move || {
        crate::lock::with_state_lock(|_| {
            held_tx.send(()).expect("report the hold");
            let _ = release_rx.recv_timeout(Duration::from_secs(6));
            Ok(())
        })
        .expect("hold the state flock");
    });
    held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the state flock is held");

    let started = std::time::Instant::now();
    daemon.tick();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the tick waited {took:?} on a lock the adoption needs"
    );

    release_tx.send(()).expect("release the flock");
    flock_holder.join().expect("flock holder");
    rot_release_tx.send(()).expect("release the rotation lock");
    rotation_holder.join().expect("rotation holder");
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
}

/// Once the last job naming the tree is gone (`claude rm`
/// deletes its job dir), the daemon drops its hold and the session ends: no
/// row, no watchdog, the tree and its marker dir collected.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_releases_an_adopted_with_fallback_session_once_its_job_is_removed() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let sessions = with_fallback_marker("wfl", &sid)
        .parent()
        .expect("marker dir")
        .to_path_buf();
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    std::fs::remove_dir_all(job_naming(&sb, &tree)).expect("claude rm");
    tick_until(&mut daemon, "released the session", |_| {
        !tree.exists() && !sessions.exists()
    });
    assert!(
        crate::live_sessions::get(&sid).is_none(),
        "the row went with the hold"
    );
    assert_eq!(watchdog_threads("wfl"), 0, "no watchdog is still running");
    assert_eq!(watchdog_threads("wfs"), 0, "no watchdog is still running");
}

/// A daemon that stops while a job still names the tree hands
/// the session on again, and the next daemon (its boot GC first) holds it.
#[cfg(target_os = "linux")]
#[test]
fn a_fresh_daemon_re_adopts_a_with_fallback_session_its_predecessor_held() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut first = daemon_for(config.clone());
    tick_until(&mut first, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    drop(first);
    assert!(tree.is_dir(), "the job keeps the tree past the daemon");
    assert!(
        crate::live_sessions::get(&sid).is_some_and(|row| row.follows_chain),
        "the stopped daemon hands the chain row on"
    );
    assert!(
        !crate::runtime::session_row_is_live(
            &crate::profile::ProfileName::from("wfl"),
            false,
            &sid
        ),
        "the old daemon's marker died with it"
    );

    crate::runtime::gc_stale_runtimes();
    let mut second = daemon_for(config);
    tick_until(&mut second, "re-adopted the session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
}

/// A job-named per-session tree is no evidence of a chain
/// session. A tree a job names with no row (its plain session's teardown
/// unregistered it), or with a row that never opted into the chain, is spared
/// and never adopted: the daemon would otherwise start moving a session
/// nobody opted in across accounts.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_never_adopts_a_job_named_tree_without_a_chain_row() {
    let sb = HomeSandbox::new();
    let config = with_fallback_chain();
    let profile = crate::profile::profile_dir(&crate::profile::ProfileName::from("wfl"))
        .expect("profile dir");
    let mut trees = Vec::new();
    for (sid, job, row) in [
        ("777002-0", "b6a1c0d3", false),
        ("777003-0", "b6a1c0d4", true),
    ] {
        let tree = profile.join(format!("runtime-{sid}"));
        std::fs::create_dir_all(&tree).expect("tree");
        std::fs::create_dir_all(profile.join(format!("sessions-{sid}"))).expect("sessions dir");
        // A real session's credential link, so the tree is adoptable in every
        // respect but the chain row: only that check can keep it unadopted.
        std::os::unix::fs::symlink(
            crate::claude::install_source_path(&crate::profile::ProfileName::from("wfl"))
                .expect("install source"),
            tree.join(".credentials.json"),
        )
        .expect("link the tree's credentials");
        crate::testutil::write_claude_job_in(sb.home(), job, &tree, "working", None);
        if row {
            let mut plain = crate::testutil::live_row(sid, "wfl");
            plain.follows_chain = false;
            crate::live_sessions::register(&plain).expect("register the plain row");
        }
        trees.push((sid, tree));
    }
    let mut daemon = daemon_for(config);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        daemon.tick();
        std::thread::sleep(Duration::from_millis(100));
    }

    for (sid, tree) in &trees {
        assert!(
            crate::live_sessions::get(sid).is_none_or(|row| row.pid != std::process::id()),
            "{sid}: the daemon registered no row of its own"
        );
        assert!(tree.is_dir(), "{sid}: the tree is spared");
    }
    assert!(
        !crate::runtime::has_live_marker(&crate::profile::ProfileName::from("wfl")),
        "no marker is held under the account"
    );
    assert_eq!(
        watchdog_threads("wfl"),
        0,
        "no watchdog runs for the account"
    );
    assert_eq!(
        daemon.background.adoption_attempts, 0,
        "no row outside the chain is ever queued, so none retakes the locks on a backoff"
    );
}

/// A launcher whose last swap moved the link but never recorded it leaves a
/// tree whose credential link names another account than the row does. The
/// daemon cannot tell which account the session spends, so it never adopts
/// that tree and never touches its link.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_never_adopts_a_tree_whose_link_names_another_account() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let link = tree.join(".credentials.json");
    std::fs::remove_file(&link).expect("unlink");
    let wfs_source = crate::claude::install_source_path(&crate::profile::ProfileName::from("wfs"))
        .expect("install source");
    std::os::unix::fs::symlink(&wfs_source, &link).expect("link onto wfs, unrecorded");
    let mut daemon = daemon_for(config);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        daemon.tick();
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        !daemon
            .background
            .held
            .contains_key(&super::BackgroundKey::Session(sid.clone())),
        "the daemon holds no session whose account it cannot tell"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("the link"),
        wfs_source,
        "the link stays as the launcher left it"
    );
    assert!(
        !crate::runtime::session_row_is_live(
            &crate::profile::ProfileName::from("wfl"),
            false,
            &sid
        ) && !crate::runtime::session_row_is_live(
            &crate::profile::ProfileName::from("wfs"),
            false,
            &sid
        ),
        "no marker of the session is held"
    );
}

/// `clauth switch <sid>` on
/// a handed-off chain session no daemon holds yet says it waits for a daemon,
/// never that it stopped. Control: once no job keeps the session, the row is
/// dead and reads as before.
#[cfg(target_os = "linux")]
#[test]
fn switching_a_handed_off_session_no_daemon_holds_names_the_wait() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");

    let waiting =
        crate::sessions_cli::run_switch(&sid, "wfs").expect_err("no daemon holds the session");
    assert_eq!(
        waiting.to_string(),
        format!(
            "session '{sid}' is a background session no clauth daemon holds yet\n\
             it follows the chain once `clauth daemon` runs"
        )
    );

    std::fs::remove_dir_all(job_naming(&sb, &tree)).expect("claude rm");
    let dead = crate::sessions_cli::run_switch(&sid, "wfs").expect_err("a dead row");
    assert_eq!(
        dead.to_string(),
        format!(
            "session '{sid}' is no longer running\n\
             its row is reaped by the next `clauth daemon` or `clauth resume`"
        )
    );
}

/// Pose the launcher's own swap onto `member` before it exited: the tree's
/// credential link on `member`'s install source, the row's current member
/// `member`.
#[cfg(target_os = "linux")]
fn pose_launcher_swap(tree: &std::path::Path, sid: &str, member: &str) {
    let link = tree.join(".credentials.json");
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(
        crate::claude::install_source_path(&crate::profile::ProfileName::from(member))
            .expect("install source"),
        &link,
    )
    .expect("link onto the member");
    crate::live_sessions::update_as_session(sid, |fields| fields.set_current_member(member))
        .expect("record the launcher's move");
}

/// Rewrite the `state` of the job record that names `tree`.
#[cfg(target_os = "linux")]
fn set_job_state(sb: &HomeSandbox, tree: &std::path::Path, state: &str) {
    let record = job_naming(sb, tree).join("state.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).expect("job record")).expect("parse");
    value["state"] = serde_json::Value::String(state.to_string());
    std::fs::write(&record, serde_json::to_vec(&value).expect("serialize")).expect("rewrite");
}

/// A real background job keeps its own Claude Code supervisor running in its
/// tree for as long as the job lives, so the adoption cannot wait for it to
/// exit: the daemon holds the session beside its live supervisor and leaves
/// that supervisor running.
#[cfg(target_os = "linux")]
#[test]
fn a_daemon_adopts_a_handed_off_session_while_its_jobs_supervisor_runs() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut supervisor = crate::testutil::FakeSupervisor::spawn(&tree, false);
    crate::testutil::write_daemon_lock(&tree, supervisor.pid, &supervisor.proc_start);

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the session beside its supervisor", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    assert!(
        matches!(supervisor.child.try_wait(), Ok(None)),
        "the adoption leaves the job's supervisor running"
    );
    drop(daemon);
}

/// The account a handed-off session runs on stays a live session's account
/// whether or not a daemon holds it yet, so it cannot be deleted, renamed or
/// disabled under the session (`has_live_session` gates all three); the
/// launch account stays live through the job naming its tree. Once the job is
/// gone and the daemon released the session, neither is live.
#[cfg(target_os = "linux")]
#[test]
fn a_handed_off_session_keeps_the_account_it_runs_on_live() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    pose_launcher_swap(&tree, &sid, "wfs");
    let wfl = crate::profile::ProfileName::from("wfl");
    let wfs = crate::profile::ProfileName::from("wfs");

    assert!(
        crate::runtime::has_live_session(&wfs),
        "no daemon holds the session yet, and its account is still in use"
    );
    assert!(crate::runtime::has_live_session(&wfl));

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the session on wfs", |d| {
        daemon_holds_session(d, &sid, "wfs")
    });
    assert!(
        crate::runtime::has_live_session(&wfs),
        "the daemon holds the session on wfs"
    );

    std::fs::remove_dir_all(job_naming(&sb, &tree)).expect("claude rm");
    tick_until(&mut daemon, "released the session", |_| !tree.exists());
    assert!(!crate::runtime::has_live_session(&wfs));
    assert!(!crate::runtime::has_live_session(&wfl));
}

/// An adopted session is one session on the account it runs on, and the
/// account surfaces count it: unlike the account's fixed background tree,
/// which the daemon holds once however many jobs run in it, a per-launch hold
/// is exactly one session.
#[cfg(target_os = "linux")]
#[test]
fn an_adopted_with_fallback_session_counts_as_a_session_on_its_account() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (_tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config.clone());
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });

    let tally = crate::live_sessions::LiveTally::collect(&config);
    let wfl = tally.member(&crate::profile::ProfileName::from("wfl"));
    assert_eq!(
        (wfl.sessions, wfl.following),
        (1, 1),
        "one session on wfl, and the chain may move it"
    );
}

/// The macOS rotation gate: rotating the account an adopted session runs on
/// would hand Claude Code's namespaced Keychain item a spent refresh token, so
/// while the session's job is unfinished the account's rotation is refused,
/// on the account the session moved onto too. A stopped job frees it, as it
/// does for every background tree.
#[cfg(target_os = "linux")]
#[test]
fn an_adopted_session_blocks_a_macos_rotation_of_its_account_while_its_job_runs() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut daemon, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    let wfs = crate::profile::ProfileName::from("wfs");

    assert!(
        crate::runtime::rotation_block_on(&wfs, true).is_some(),
        "the session's unfinished job blocks rotating the account it runs on"
    );
    assert_eq!(
        crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfl"), true),
        None,
        "the session left wfl, whose dir its tree still sits in: rotating wfl strands nothing"
    );

    set_job_state(&sb, &tree, "stopped");
    assert_eq!(
        crate::runtime::rotation_block_on(&wfs, true),
        None,
        "a stopped job frees the rotation"
    );
}

/// The registry rows of handed-off sessions outlive the pid in their id, so a
/// new session's id never repeats one a row already names: a recycled pid
/// would otherwise overwrite the hand-off record with a new session's.
#[cfg(target_os = "linux")]
#[test]
fn a_new_session_never_takes_the_id_a_handed_off_row_names() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (_tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let handed_off = serde_json::to_value(crate::live_sessions::get(&sid)).expect("serialize");

    let seq = sid
        .rsplit_once('-')
        .and_then(|(_, seq)| seq.parse::<u64>().ok())
        .expect("a <pid>-<seq> id");
    let daemon_lock = crate::daemon::hold_daemon_lock();
    crate::runtime::rewind_session_seq_for_test(seq);
    crate::start::run(
        &config,
        &crate::profile::ProfileName::from("wfl"),
        &["--bg".to_string()],
        crate::runtime::Isolation::Shared,
        None,
        true,
        None,
    )
    .expect("a second --with-fallback start");
    drop(daemon_lock);

    assert_eq!(
        serde_json::to_value(crate::live_sessions::get(&sid)).expect("serialize"),
        handed_off,
        "the hand-off record is untouched"
    );
    let trees: Vec<String> = fake
        .spawns()
        .into_iter()
        .filter(|(_, argv)| argv.split(' ').any(|word| word == "--bg"))
        .map(|(dir, _)| dir)
        .collect();
    assert_eq!(trees.len(), 2, "two launches");
    assert_ne!(trees[0], trees[1], "the second launch ran on its own tree");
}

/// A clauth that changed the account's rolling-token setup while the session
/// was handed off moves the account's install source between its two stores;
/// the session's link still names the other one. That is the account's own
/// store, so the daemon adopts the session, and its convergence moves the link
/// onto the current install source.
#[cfg(target_os = "linux")]
#[test]
fn an_adoption_accepts_a_link_on_the_accounts_other_store_and_converges_it() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let wfl = crate::profile::ProfileName::from("wfl");
    crate::claude::write_session_token(&wfl, "oat-mint", 0).expect("write the sidecar");
    let sidecar = member_store_of("wfl");
    assert_ne!(
        tree_store(&tree).as_ref(),
        Some(&sidecar),
        "the fixture moved the install source off the link"
    );

    let mut daemon = daemon_for(config);
    tick_until(
        &mut daemon,
        "held the session and converged its link",
        |d| daemon_holds_session(d, &sid, "wfl") && tree_store(&tree).as_ref() == Some(&sidecar),
    );
}

/// An adoption claims the session's marker before it writes anything: with
/// another holder keeping that marker, it refuses and leaves the tree's
/// settings and the hand-off row exactly as they were.
#[cfg(target_os = "linux")]
#[test]
fn an_adoption_refused_on_a_held_marker_writes_nothing() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let held = crate::runtime::open_pid_file(&with_fallback_marker("wfl", &sid)).expect("marker");
    held.lock().expect("another holder keeps the marker");
    let settings = tree.join("settings.json");
    std::fs::write(&settings, b"{\"sentinel\":true}").expect("sentinel settings");
    let before = serde_json::to_value(crate::live_sessions::get(&sid)).expect("serialize");

    let refused = crate::runtime::ProfileRuntime::acquire_adopted(&sid, &[]);

    assert!(refused.is_err(), "the marker is held elsewhere");
    assert_eq!(
        std::fs::read(&settings).expect("settings"),
        b"{\"sentinel\":true}",
        "the tree is untouched"
    );
    assert_eq!(
        serde_json::to_value(crate::live_sessions::get(&sid)).expect("serialize"),
        before,
        "the row is untouched"
    );
    drop(held);
}

/// The adopted session keeps judging chain members against what it booted
/// with, never against the launch account's config as edited since: an env
/// added to `wfl` after the launch must not make `wfs` (still env-equal to
/// the boot) ineligible.
#[cfg(target_os = "linux")]
#[test]
fn an_adopted_session_judges_swaps_against_what_it_booted_with() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let wfl = crate::profile::ProfileName::from("wfl");
    let mut edited = crate::profile::read_profile(&wfl).expect("read wfl");
    edited
        .env
        .insert("WFL_EDITED_AFTER_LAUNCH".to_string(), "1".to_string());
    save_profile(&edited).expect("save the edit");

    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut daemon, "moved the session onto wfs", |_| {
        tree_store(&tree).as_ref() == Some(&wfs_store)
    });
}

/// On macOS a move's Keychain write lands after its link and row moves, and
/// until it does the session still reads the account it left: the row's
/// `launch_store` names that account's store while the link and the current
/// member already name the new one. The rotation gate follows `launch_store`,
/// the store the session reads, so it keeps the left account's rotation
/// refused and leaves the new one free.
#[cfg(target_os = "linux")]
#[test]
fn the_rotation_gate_follows_the_store_the_session_reads() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut daemon, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    let wfl_store = crate::claude::install_source_path(&crate::profile::ProfileName::from("wfl"))
        .expect("wfl store");
    crate::live_sessions::update_as_session(&sid, |fields| fields.set_launch_store(wfl_store))
        .expect("pose the Keychain write that has not landed");

    assert_eq!(
        (
            crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfl"), true)
                .is_some(),
            crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfs"), true)
                .is_some(),
        ),
        (true, false),
        "(wfl blocked, wfs blocked): the session still reads wfl's store"
    );
}

/// A store the session's row names but that no longer exists (a static token
/// cleared from under it) leaves the gate unable to tell which store the
/// session reads, so it refuses rotation of the accounts the row names, and
/// only those: an account the session never ran on rotates as before.
#[cfg(target_os = "linux")]
#[test]
fn an_unreadable_session_store_blocks_only_the_accounts_its_row_names() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    save_profile(&profile_with_creds("wfu", "tok-wfu")).expect("an account outside the session");
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut daemon, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    let gone = crate::profile::profile_dir(&crate::profile::ProfileName::from("wfs"))
        .expect("profile dir")
        .join("session-token.json");
    assert!(!gone.exists(), "the fixture's store is absent");
    crate::live_sessions::update_as_session(&sid, |fields| fields.set_launch_store(gone))
        .expect("pose a cleared store");

    assert!(
        crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfs"), true)
            .is_some(),
        "the account the session runs on stays refused"
    );
    assert_eq!(
        crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfu"), true),
        None,
        "an account the session never ran on rotates"
    );
}

/// A move whose Keychain write never landed leaves the session reading a
/// third account's store, named only by its `launch_store` path; once that
/// store is cleared the path no longer resolves, and the account whose dir
/// holds it is still the one a rotation would strand, so it stays refused.
#[cfg(target_os = "linux")]
#[test]
fn an_unresolved_store_under_a_third_account_blocks_that_account() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    save_profile(&profile_with_creds("wfu", "tok-wfu")).expect("a third account");
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut daemon = daemon_for(config);
    tick_until(&mut daemon, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut daemon, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    let gone = crate::profile::profile_dir(&crate::profile::ProfileName::from("wfu"))
        .expect("profile dir")
        .join("session-token.json");
    assert!(!gone.exists(), "the fixture's store is absent");
    crate::live_sessions::update_as_session(&sid, |fields| fields.set_launch_store(gone))
        .expect("pose an unlanded move onto a cleared wfu store");

    assert!(
        crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfu"), true)
            .is_some(),
        "the account whose cleared store the session reads stays refused"
    );
}

/// A row with no recorded store and a credential link that resolves nowhere
/// gives the gate nothing to read but the row itself: the account the session
/// runs on stays refused, and an account it never ran on rotates.
#[cfg(target_os = "linux")]
#[test]
fn a_row_with_no_store_and_a_dangling_link_blocks_its_current_account_only() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    save_profile(&profile_with_creds("wfu", "tok-wfu")).expect("an account outside the session");
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    // Handed off, never held: no watchdog converges the link under the test.
    let mut row = crate::live_sessions::get(&sid).expect("the hand-off row");
    row.current_member = Some("wfs".to_string());
    row.launch_store = None;
    crate::live_sessions::register(&row).expect("pose a row with no recorded store");
    let link = tree.join(".credentials.json");
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(tree.join("no-such-store.json"), &link).expect("dangling link");

    assert!(
        crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfs"), true)
            .is_some(),
        "the account the session runs on stays refused"
    );
    assert_eq!(
        crate::runtime::rotation_block_on(&crate::profile::ProfileName::from("wfu"), true),
        None,
        "an account the session never ran on rotates"
    );
}

/// The live Claude Code process keeps every refresh token it was ever handed,
/// so every account a session has run on stays a live session's account until
/// the session ends, not only the one it runs on now: through a hand-off with
/// no daemon, and once a fresh daemon holds the session again.
#[cfg(target_os = "linux")]
#[test]
fn a_handed_off_session_keeps_every_account_it_ran_on_live() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut first = daemon_for(config.clone());
    tick_until(&mut first, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut first, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    crate::sessions_cli::run_switch(&sid, "wfl").expect("switch back onto wfl");
    let wfl_store = member_store_of("wfl");
    tick_until(&mut first, "moved the session back onto wfl", |d| {
        tree_store(&tree).as_ref() == Some(&wfl_store) && daemon_holds_session(d, &sid, "wfl")
    });
    let wfs = crate::profile::ProfileName::from("wfs");
    assert!(
        crate::runtime::has_live_session(&wfs),
        "the held session keeps the account it left"
    );

    drop(first);
    assert!(
        crate::runtime::has_live_session(&wfs),
        "the handed-off session still keeps the account it left"
    );

    let mut second = daemon_for(config);
    tick_until(&mut second, "re-adopted the session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    assert!(
        crate::runtime::has_live_marker(&wfs),
        "the re-adopted session holds a marker under the account it left"
    );
}

/// Another live holder on the marker of an account the session ran on (a
/// sibling process still tearing it down) refuses the adoption for a retry,
/// and a refused attempt leaves no marker file of its own behind; once the
/// holder lets go, the daemon holds the session.
#[cfg(target_os = "linux")]
#[test]
fn a_held_marker_of_a_visited_account_refuses_the_adoption_cleanly() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut first = daemon_for(config.clone());
    tick_until(&mut first, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    crate::sessions_cli::run_switch(&sid, "wfs").expect("switch onto wfs");
    let wfs_store = member_store_of("wfs");
    tick_until(&mut first, "moved the session onto wfs", |d| {
        tree_store(&tree).as_ref() == Some(&wfs_store) && daemon_holds_session(d, &sid, "wfs")
    });
    crate::sessions_cli::run_switch(&sid, "wfl").expect("switch back onto wfl");
    let wfl_store = member_store_of("wfl");
    tick_until(&mut first, "moved the session back onto wfl", |d| {
        tree_store(&tree).as_ref() == Some(&wfl_store) && daemon_holds_session(d, &sid, "wfl")
    });
    drop(first);
    let visited = with_fallback_marker("wfs", &sid);
    std::fs::create_dir_all(visited.parent().expect("marker dir")).expect("the holder's dir");
    let held = crate::runtime::open_pid_file(&visited).expect("visited marker");
    held.lock()
        .expect("another holder keeps the visited marker");

    let mut second = daemon_for(config);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        second.tick();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !second
            .background
            .held
            .contains_key(&super::BackgroundKey::Session(sid.clone())),
        "the daemon holds no session another process still holds a marker of"
    );
    assert!(
        !with_fallback_marker("wfl", &sid).exists(),
        "a refused attempt leaves no marker file of its own"
    );

    drop(held);
    tick_until(
        &mut second,
        "held the session once the holder let go",
        |d| daemon_holds_session(d, &sid, "wfl"),
    );
}

/// An attempt that claimed one visited account's marker and then meets
/// another holder on the next one rolls back what it made: the marker file and
/// the `sessions-<sid>/` dir it created under the first account are gone, and
/// the holder's own file stays.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_adoption_rolls_back_the_markers_it_claimed_first() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let mut config = persist(
        vec![
            profile_with_creds("wfl", "tok-wfl"),
            profile_with_creds("wfs", "tok-wfs"),
            profile_with_creds("wft", "tok-wft"),
        ],
        None,
        60_000,
    );
    config.state.fallback_chain = vec!["wfl".into(), "wfs".into(), "wft".into()];
    save_app_state(&config.state).expect("persist the chain");
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    let mut first = daemon_for(config.clone());
    tick_until(&mut first, "held the handed-off session", |d| {
        daemon_holds_session(d, &sid, "wfl")
    });
    for member in ["wfs", "wft", "wfl"] {
        crate::sessions_cli::run_switch(&sid, member).expect("switch along the chain");
        let store = member_store_of(member);
        tick_until(&mut first, "moved the session", |d| {
            tree_store(&tree).as_ref() == Some(&store) && daemon_holds_session(d, &sid, member)
        });
    }
    drop(first);
    let first_visited = with_fallback_marker("wfs", &sid);
    assert!(
        !first_visited.exists(),
        "the handed-off session's markers went with the daemon that held it"
    );
    let foreign = with_fallback_marker("wft", &sid);
    std::fs::create_dir_all(foreign.parent().expect("marker dir")).expect("the holder's dir");
    let held = crate::runtime::open_pid_file(&foreign).expect("foreign marker");
    held.lock().expect("another holder keeps wft's marker");

    let mut second = daemon_for(config);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        second.tick();
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        !second
            .background
            .held
            .contains_key(&super::BackgroundKey::Session(sid.clone())),
        "the daemon holds no session another process still holds a marker of"
    );
    assert!(
        !first_visited.exists() && !first_visited.parent().expect("marker dir").exists(),
        "the attempt rolled back the wfs marker file and dir it created"
    );
    assert!(foreign.is_file(), "the other holder's file stays");
    drop(held);
}

/// The switch line's wait copy needs a job that names the tree: under a job
/// store that cannot be read, a dead chain row reads as dead, never as a
/// session waiting for a daemon.
#[cfg(target_os = "linux")]
#[test]
fn switching_under_an_unreadable_job_store_never_claims_a_waiting_session() {
    let sb = HomeSandbox::new();
    let _claude = with_fallback_claude_home(&sb);
    let fake = crate::testutil::FakeClaude::new(&sb);
    let config = with_fallback_chain();
    let (tree, sid) = launch_with_fallback(&fake, &config, "--bg");
    std::fs::remove_dir_all(job_naming(&sb, &tree)).expect("claude rm");
    let torn = sb.home().join(".claude").join("jobs").join("b6a1c0d5");
    std::fs::create_dir_all(&torn).expect("torn job dir");
    std::fs::write(torn.join("state.json"), b"{ torn").expect("torn record");

    let dead = crate::sessions_cli::run_switch(&sid, "wfs").expect_err("a dead row");
    assert_eq!(
        dead.to_string(),
        format!(
            "session '{sid}' is no longer running\n\
             its row is reaped by the next `clauth daemon` or `clauth resume`"
        )
    );
}

/// The drain a stopping daemon runs is built from the daemon's OWN scheduler
/// store and shutdown flag: a spend the scheduler claims holds the drain, and
/// the drain's flag is the one the scheduler's refusals read.
#[cfg(unix)]
#[test]
fn a_daemons_spend_drain_waits_on_its_own_schedulers_claims() {
    let _home = HomeSandbox::new();
    let daemon = daemon_for(persist(vec![], None, 60_000));
    let name = crate::profile::ProfileName::from("mid-spend");
    let held = crate::usage::claim_scheduler_spend_unless_shutting_down(
        &daemon.activity,
        &name,
        &daemon.shutting_down,
    )
    .expect("an unset flag admits the spend");

    let drain = daemon.spend_drain();

    assert!(
        !drain.drain(Duration::from_millis(300)),
        "the scheduler's claim holds the drain"
    );
    assert!(
        daemon.shutting_down.load(Ordering::SeqCst),
        "the drain raised the scheduler's own flag"
    );
    drop(held);
}

/// A queued switch whose AUTH-1 gate is waiting on the target's rotation lock
/// when the stop signal arrives sends nothing once the lock frees: the stop
/// flag is read after the wait, right before the token request, so a gate that
/// was parked behind another holder never starts a spend the exit could cut.
#[test]
fn a_gate_parked_on_the_rotation_lock_sends_nothing_once_stopping() {
    let home = HomeSandbox::new();
    let beta = expired_profile_with_creds("beta", "at-beta");
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha"), beta],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (
            200,
            r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#.to_string(),
        )
    });
    let endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut daemon = daemon_for(config);
    stage_switch(&daemon, "beta");
    let holder = crate::testutil::hold_rotation_lock("beta");
    // The gate reports reaching its rotation-lock wait instead of the test
    // guessing a wall-clock delay: the stop flag then always lands after the
    // park, on any runner speed.
    let flag = std::sync::Arc::clone(&daemon.shutting_down);
    let releaser = crate::testutil::stop_when_parked(flag, holder);

    daemon.drain_pending_switch();
    crate::oauth::set_rotation_park_hook(None);
    releaser.join().expect("the releaser ran");

    drop(endpoints);
    assert_eq!(
        server.join().expect("listener"),
        Vec::<String>::new(),
        "a gate that waited out the stop signal sends no token request"
    );
    assert_eq!(active_of(&daemon).as_deref(), Some("alpha"));
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the refused switch stays queued"
    );
    assert!(
        daemon.switch_backoff.is_none(),
        "a refusal at stop sets no backoff"
    );
}

// ── E2E `serve` stop ─────────────────────────────────────────────────────────

/// The re-entry target for the E2E `serve` stop tests: with the marker env
/// absent (the normal suite run) it passes instantly; with it present it seeds
/// one OAuth profile whose access token is already expired, points the token
/// endpoint at the parent's loopback stub, and runs the real [`serve`] until
/// the signal its test sends kills the process or its parent dies.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn daemon_serve_child_for_stop_tests() {
    let Some(home) = std::env::var_os(crate::testutil::SERVE_STOP_CHILD_HOME_ENV) else {
        return;
    };
    // A SIGKILLed fixture parent runs no `Drop`: watch the pid this child
    // started under and exit once the reparent moves it, so no `serve` process
    // outlives its fixture parent.
    let parent = std::os::unix::process::parent_id();
    let _watcher = std::thread::spawn(move || crate::testutil::exit_when_reparented(parent));
    let base = std::env::var(crate::testutil::SERVE_STOP_TOKEN_BASE_ENV)
        .expect("the parent stubbed the token endpoint");
    let home = std::path::PathBuf::from(home);
    crate::profile::set_home_override(home);
    crate::oauth::set_endpoint_overrides(
        &format!("{base}/v1/oauth/token"),
        &format!("{base}/v1/messages?beta=true"),
    );
    crate::usage::set_usage_endpoint_override(
        &format!("{base}/api/oauth/usage"),
        &format!("{base}/api/oauth/profile"),
    );
    // One expired access token: the scheduler's poll rotation spends its
    // refresh token at once, which is the in-flight save the stop must wait out.
    let profile = expired_profile_with_creds("alpha", "at-expired");
    persist(vec![profile], Some("alpha"), 90_000);
    let _ = super::serve(
        super::StartMode::ExitIfRunning,
        None,
        &super::api::tls::CertSource::Lego,
    );
}

/// The real [`serve`] waits out the scheduler's token spend before dying of the
/// SIGTERM a stop sent: a child holding an in-flight refresh gets SIGTERM, the
/// drain refuses new spends and waits (bounded by `SPEND_DRAIN_BOUND`) for the
/// one already in flight to save, and the minted pair lands in the profile
/// store before the process dies of that same signal. Driving `serve`
/// end-to-end pins that the drain is built over the daemon's OWN scheduler
/// store — a drain built over a fresh store would see no spend, log no wait,
/// and the child would die without the save.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn serve_waits_out_an_inflight_token_save_before_dying_of_sigterm() {
    crate::testutil::serve_stop_e2e(
        |pid, _home| {
            std::thread::spawn(move || {
                std::process::Command::new("kill")
                    .args(["-s", "TERM", &pid.to_string()])
                    .status()
                    .expect("send SIGTERM");
            })
        },
        std::time::Duration::ZERO,
        "the daemon still dies of the SIGTERM it got",
    );
}

/// A SIGKILLed fixture parent runs no `Drop`: the re-entered `serve` child has
/// to notice its own reparent and exit, or it serves on forever, holding the
/// test's loopback stub and the child home.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_serve_child_exits_when_its_parent_is_sigkilled() {
    let _home = HomeSandbox::new();
    let child_home = tempfile::tempdir().expect("child home");
    let scratch = tempfile::tempdir().expect("test scratch");
    let stub = crate::testutil::TokenStub::new(scratch.path().join("answered"));
    let log = scratch.path().join("daemon.log");
    // The stub's answered marker is the child's readiness signal: the scheduler
    // reached the token endpoint, so the child is well past its first parent
    // read.
    let (mut proxy, child) = crate::testutil::spawn_fixture_under_proxy(
        "serve",
        stub.answered(),
        child_home.path(),
        &stub.base,
        Some(&log),
    );
    let start = crate::testutil::proc_start_of(child).expect("the serve child's start");
    proxy.0.kill().expect("SIGKILL the fixture parent");
    let _ = proxy.0.wait();
    crate::testutil::assert_child_exits(child, &start, "the serve child");
    stub.release();
    let _ = stub.finish();
}

/// The parked pid-stop fixture is the other re-entry target: a SIGKILLed
/// parent must not leave it sleeping out its full hour either.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_parked_fixture_child_exits_when_its_parent_is_sigkilled() {
    let _home = HomeSandbox::new();
    let config_home = tempfile::tempdir().expect("child config dir");
    let ready = config_home.path().join("ready");
    let (mut proxy, child) =
        crate::testutil::spawn_fixture_under_proxy("park", &ready, config_home.path(), "", None);
    let start = crate::testutil::proc_start_of(child).expect("the parked child's start");
    proxy.0.kill().expect("SIGKILL the fixture parent");
    let _ = proxy.0.wait();
    crate::testutil::assert_child_exits(child, &start, "the parked fixture child");
}

// ── reload operator-log + stale-bearer pins ──────────────────────────────────

fn reload_lines(daemon: &mut Daemon) -> Vec<String> {
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    daemon.reload_if_changed();
    lines.snapshot()
}

/// How many times `log` holds the reload line, matched whole.
fn reload_count(log: &[String]) -> usize {
    log.iter()
        .filter(|l| l.as_str() == "clauth daemon: reloaded config")
        .count()
}

/// An external `profiles.toml` edit that moves the active marker and the
/// fallback chain, and no bearer, reloads and logs the reload once.
#[test]
fn an_external_chain_and_active_edit_logs_the_reload() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("a", "at-a"),
            profile_with_creds("b", "at-b"),
        ],
        Some("a"),
        90_000,
    );
    let mut daemon = daemon_for(config);
    let external = AppState {
        active_profile: Some("b".into()),
        profiles: vec!["a".into(), "b".into()],
        fallback_chain: vec!["b".into(), "a".into()],
        refresh_interval_ms: 90_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external write");
    let state_path = clauth_dir().unwrap().join("profiles.toml");
    set_mtime(&state_path, SystemTime::now() + Duration::from_secs(5));
    let log = reload_lines(&mut daemon);
    assert_eq!(active_of(&daemon).as_deref(), Some("b"), "reloaded");
    assert_eq!(
        reload_count(&log),
        1,
        "external chain/active edit logs: {log:?}"
    );
}

/// Break a config, repair it, then break it again with the same text: the
/// second outage must log too. Pins that the failed-load dedupe resets on a
/// successful reload, so a recurring identical parse error never goes silent.
#[test]
fn a_second_identical_config_break_logs_again() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("a", "at-a"),
            profile_with_creds("b", "at-b"),
        ],
        Some("a"),
        90_000,
    );
    let mut daemon = daemon_for(config);
    let b_cfg =
        crate::profile::profile_subpath(&crate::profile::ProfileName::from("b"), "config.toml")
            .expect("path");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    std::fs::write(&b_cfg, "not = [valid toml").unwrap();
    set_mtime(&b_cfg, SystemTime::now() + Duration::from_secs(5));
    daemon.reload_if_changed();
    std::fs::write(&b_cfg, "").unwrap();
    set_mtime(&b_cfg, SystemTime::now() + Duration::from_secs(10));
    daemon.reload_if_changed();
    assert_eq!(
        daemon.last_reload_fp,
        reload_fingerprint(),
        "repaired reload adopted"
    );
    std::fs::write(&b_cfg, "not = [valid toml").unwrap();
    set_mtime(&b_cfg, SystemTime::now() + Duration::from_secs(15));
    daemon.reload_if_changed();
    let log = lines.snapshot();
    assert_eq!(
        log.iter()
            .filter(|l| l.contains("config reload failed"))
            .count(),
        2,
        "each outage logs: {log:?}"
    );
}

/// End to end through `Daemon::tick`: a queued switch, then a copy-back on an
/// anchored profile between ticks, then the next `/usage` bearer. Pins that the
/// copy-back tick logs one reload and every `/usage` bearer is the stored token.
#[test]
fn the_tick_loop_switch_then_copy_back_follows_the_store() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let (base, server) = crate::testutil::serve_endpoints_raw(8, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (
                200,
                r#"{"five_hour":{"utilization":1.0,"resets_at":"2099-01-01T00:00:00+00:00"}}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&_home, &base);
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
            profile_with_creds("gamma", "at-G1"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let gamma = crate::profile::ProfileName::from("gamma");
    crate::usage::seed_login_anchor(&gamma, Some(&crate::profile::AccountId::from("uuid-G")));
    let mut daemon = daemon_for(config);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    stage_switch(&daemon, "beta");
    daemon.tick();
    assert_eq!(active_of(&daemon).as_deref(), Some("beta"), "switch landed");
    let after_switch = lines.snapshot();

    let cred = crate::profile::profile_subpath(&gamma, "credentials.json").expect("cred");
    crate::profile::atomic_write_600(
        &cred,
        serde_json::to_vec(&oauth_creds("at-G2")).expect("encode"),
    )
    .expect("copy-back");
    daemon.tick();
    let after_copy = lines.snapshot();

    let state = crate::usage::SchedulerState::over_shared(
        daemon.config.clone(),
        daemon.usage_tokens.clone(),
        daemon.usage_store.clone(),
        daemon.usage_status.clone(),
        daemon.last_fetched.clone(),
        daemon.poll_streaks.clone(),
        daemon.snapshot.clone(),
    );
    crate::usage::tick(&state);
    drop(_endpoints);
    let requests = server.join().expect("listener");
    let mut bearers: Vec<String> = requests
        .iter()
        .filter(|raw| crate::testutil::request_path(raw).starts_with("/api/oauth/usage"))
        .filter_map(|raw| crate::testutil::request_header(raw, "authorization"))
        .collect();
    bearers.sort();
    assert_eq!(
        reload_count(&after_copy) - reload_count(&after_switch),
        1,
        "the copy-back tick logs one reload: {after_copy:?}"
    );
    assert_eq!(
        bearers,
        vec![
            "Bearer at-G2".to_string(),
            "Bearer at-alpha".to_string(),
            "Bearer at-beta".to_string()
        ],
        "every /usage bearer is the stored token"
    );
}

/// The scheduler worker's own rotation persist lands before its outcome drain
/// syncs the token list; a loop reload in that window rebuilds onto the new
/// token and logs the reload once, own write or not.
#[test]
fn an_own_rotations_reload_rebuilds_and_logs_once() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("a", "at-a"),
            profile_with_creds("b", "at-b"),
        ],
        Some("b"),
        90_000,
    );
    let mut daemon = daemon_for(config);
    let a = crate::profile::ProfileName::from("a");
    let landed = crate::oauth::apply_rotated_tokens_locked(
        &daemon.config,
        &a,
        &crate::codex_auth::token_fingerprint("rt-at-a"),
        &oauth_creds("at-a2"),
        "at-a",
    );
    assert!(landed.is_ok(), "the rotation persist landed");
    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("at-a"),
        "drain not yet run"
    );
    let log = reload_lines(&mut daemon);
    assert_eq!(
        daemon_token(&daemon, "a").as_deref(),
        Some("at-a2"),
        "reload rebuilt"
    );
    assert_eq!(reload_count(&log), 1, "the reload logs once: {log:?}");
}

/// The daemon's boot settles the bare item as the startup relink, so its
/// refusals say `not relinking`. The settle is macOS-only, so no run on
/// another platform reaches it.
#[test]
fn the_daemon_boot_settles_as_the_startup_relink() {
    let daemon = include_str!("../../src/daemon/mod.rs");
    let boot = daemon
        .split_once("    fn boot(&self) {")
        .expect("boot defined")
        .1
        .split_once("\n    }\n")
        .expect("boot closes")
        .0;
    let settle = boot
        .split_once("crate::claude::settle_bare_keychain_item(writer)")
        .expect("boot settles the bare item")
        .0;
    let writer = settle
        .rsplit_once("let writer = ")
        .expect("boot names its writer")
        .1;
    assert!(
        writer.starts_with("crate::claude::BareItemWriter::Relink;"),
        "{writer:?}"
    );
}

/// A refused setup token stays a dead login in a freshly started daemon: it
/// seeds the verdict off the durable record, since with the probe switched off
/// nothing would probe the token and learn it anew. Only that seed keeps the
/// walk off the account: without it the walk takes it.
#[test]
fn a_started_daemon_keeps_a_refused_setup_token_off_the_walk() {
    let _home = HomeSandbox::new();
    let mut dead = profile_with_creds("dead", "at-dead");
    crate::testutil::make_probed_account(&mut dead);
    dead.usage_probe = false;
    let digest = dead.setup_token_digest.expect("the setup token's digest");
    let mut config = persist(
        vec![profile_with_creds("alpha", "at-alpha"), dead],
        Some("alpha"),
        90_000,
    );
    config.state.fallback_chain = vec!["alpha".into(), "dead".into()];
    save_app_state(&config.state).expect("persist the chain");
    link_active_clean("alpha");
    crate::profile_cache::write_auth_expired(&"dead".into(), digest);
    let daemon = daemon_for(config);

    let now = crate::usage::now_epoch_secs();
    let window = |utilization: f64| crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization,
            resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    for (name, utilization) in [("alpha", 100.0), ("dead", 10.0)] {
        daemon
            .usage_store
            .lock()
            .expect("usage")
            .insert(name.to_string(), window(utilization));
        daemon
            .usage_status
            .lock()
            .expect("status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }

    scan_once(&daemon);
    assert!(
        queued_targets(&daemon).is_empty(),
        "the walk queued a switch onto a refused setup token"
    );
    daemon.third_party_broken.lock().expect("broken").clear();
    scan_once(&daemon);
    assert_eq!(
        queued_targets(&daemon),
        vec!["dead".to_string()],
        "fixture control: without the seeded verdict the walk takes the account"
    );
}

/// A rejected api key is seeded at a daemon's start like a refused setup token,
/// through the seed the TUI runs, so the two processes start from one verdict
/// for the account.
#[test]
fn a_daemon_seeds_a_rejected_api_key_at_start() {
    let _home = HomeSandbox::new();
    let tp = crate::profile::Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("old-key".to_string()),
    );
    let fingerprint = crate::usage::dead_login_fingerprint(&tp).expect("a keyed fingerprint");
    let config = persist(
        vec![tp, profile_with_creds("oauth", "at-oauth")],
        Some("oauth"),
        90_000,
    );
    crate::profile_cache::write_auth_expired(&"tp".into(), fingerprint);
    let daemon = daemon_for(config);
    assert_eq!(
        *daemon.third_party_broken.lock().expect("broken"),
        std::collections::HashMap::from([("tp".to_string(), fingerprint)])
    );
}

/// A dead-login record removed while the daemon runs, by hand or by the
/// process that fetches usage, leaves the daemon's verdict with it on the next
/// tick: no config change follows a record, so the reload seed never looks,
/// and the daemon and the TUI would judge the account apart.
#[test]
fn a_daemon_tick_drops_a_dead_login_whose_record_is_gone() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let mut dead = profile_with_creds("dead", "at-dead");
    crate::testutil::make_probed_account(&mut dead);
    dead.usage_probe = false;
    let digest = dead.setup_token_digest.expect("the setup token's digest");
    let mut config = persist(
        vec![profile_with_creds("alpha", "at-alpha"), dead],
        Some("alpha"),
        90_000,
    );
    config.state.fallback_chain = vec!["alpha".into(), "dead".into()];
    save_app_state(&config.state).expect("persist the chain");
    link_active_clean("alpha");
    crate::profile_cache::write_auth_expired(&"dead".into(), digest);
    let mut daemon = daemon_for(config);
    assert!(
        daemon
            .third_party_broken
            .lock()
            .expect("broken")
            .contains_key("dead"),
        "control: the start seeds the verdict"
    );

    crate::profile_cache::clear_auth_expired(&"dead".into());
    daemon.tick();

    let now = crate::usage::now_epoch_secs();
    for (name, utilization) in [("alpha", 100.0), ("dead", 10.0)] {
        daemon.usage_store.lock().expect("usage").insert(
            name.to_string(),
            crate::usage::UsageInfo {
                five_hour: Some(crate::usage::UsageWindow {
                    utilization,
                    resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
                }),
                ..Default::default()
            },
        );
        daemon
            .usage_status
            .lock()
            .expect("status")
            .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
    }
    scan_once(&daemon);
    assert_eq!(queued_targets(&daemon), vec!["dead".to_string()]);
}

/// The other direction: a dead-login record another process writes while the
/// daemon runs, a TUI fetching usage while the daemon stands down say, reaches
/// the daemon's verdicts on the next tick, with no config change to reload.
#[test]
fn a_daemon_tick_takes_a_dead_login_another_process_recorded() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let mut dead = profile_with_creds("dead", "at-dead");
    crate::testutil::make_probed_account(&mut dead);
    dead.usage_probe = false;
    let digest = dead.setup_token_digest.expect("the setup token's digest");
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha"), dead],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);
    daemon.tick();
    assert!(
        daemon.third_party_broken.lock().expect("broken").is_empty(),
        "control: nothing is recorded yet"
    );

    crate::profile_cache::write_auth_expired(&"dead".into(), digest);
    daemon.tick();
    assert_eq!(
        *daemon.third_party_broken.lock().expect("broken"),
        std::collections::HashMap::from([("dead".to_string(), digest)])
    );
}

/// A daemon reload seeds a dead login another clauth process recorded after
/// the daemon started, a TUI that fetched usage while the daemon stood down
/// say, with the probe off so nothing in the daemon would learn it anew. Both
/// arms of the reload seed: the load that replaces the config, and the failed
/// load that keeps the current one in force.
#[test]
fn a_daemon_reload_seeds_a_dead_login_another_process_recorded() {
    for load_fails in [false, true] {
        let _home = HomeSandbox::new();
        let mut dead = profile_with_creds("dead", "at-dead");
        crate::testutil::make_probed_account(&mut dead);
        dead.usage_probe = false;
        let digest = dead.setup_token_digest.expect("the setup token's digest");
        let mut config = persist(
            vec![profile_with_creds("alpha", "at-alpha"), dead],
            Some("alpha"),
            90_000,
        );
        config.state.fallback_chain = vec!["alpha".into(), "dead".into()];
        save_app_state(&config.state).expect("persist the chain");
        link_active_clean("alpha");
        let mut daemon = daemon_for(config);
        assert!(
            daemon.third_party_broken.lock().expect("broken").is_empty(),
            "control: nothing is recorded at the start (load fails: {load_fails})"
        );

        crate::profile_cache::write_auth_expired(&"dead".into(), digest);
        let changed = if load_fails {
            let path = crate::profile::profile_subpath(&"alpha".into(), "config.toml")
                .expect("config path");
            std::fs::write(&path, "not = [valid toml").expect("write a broken config");
            path
        } else {
            clauth_dir().expect("clauth dir").join("profiles.toml")
        };
        set_mtime(&changed, SystemTime::now() + Duration::from_secs(5));
        daemon.reload_if_changed();

        let now = crate::usage::now_epoch_secs();
        for (name, utilization) in [("alpha", 100.0), ("dead", 10.0)] {
            daemon.usage_store.lock().expect("usage").insert(
                name.to_string(),
                crate::usage::UsageInfo {
                    five_hour: Some(crate::usage::UsageWindow {
                        utilization,
                        resets_at: Some(crate::usage::epoch_secs_to_iso(now + 3600)),
                    }),
                    ..Default::default()
                },
            );
            daemon
                .usage_status
                .lock()
                .expect("status")
                .insert(name.to_string(), crate::usage::FetchStatus::Fresh);
        }
        scan_once(&daemon);
        assert!(
            queued_targets(&daemon).is_empty(),
            "the walk queued a switch onto a dead login the reload should have seeded (load fails: {load_fails})"
        );
        daemon.third_party_broken.lock().expect("broken").clear();
        scan_once(&daemon);
        assert_eq!(
            queued_targets(&daemon),
            vec!["dead".to_string()],
            "fixture control: without the seed the walk takes the account (load fails: {load_fails})"
        );
    }
}
