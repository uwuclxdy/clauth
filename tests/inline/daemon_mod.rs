//! TECH-5 — characterization of the daemon's per-tick work (`Daemon::tick` and
//! the drains extracted to `src/daemon/tick.rs`). These PIN CURRENT behavior on
//! the top reliability path so the TECH-6 queue rewrite and TECH-7 RMW fix are
//! TDD-able rather than eyeballed (the ledger already records a `drain_config_ops`
//! mtime regression on this branch caught only by manual review).
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
    load_config, reload_fingerprint, save_app_state, save_profile,
};
use crate::testutil::{HomeSandbox, blank_profile, set_mtime};
use crate::usage::{
    Origin, PendingSwitchEntry, ProfileActivity, clear_activity, enqueue_pending_switch,
    mark_activity, now_ms,
};

use super::{ConfigOp, Daemon};

/// Push a switch request directly onto the daemon's queue (bypassing the enqueue
/// helper) so a test can stage an exact `{origin, target}` set — including a
/// {User, Scheduler} pair that would never coexist through the helper.
fn stage_switch(d: &Daemon, target: &str, origin: Origin, retry_until: u64) {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .push_back(PendingSwitchEntry {
            target: target.into(),
            origin,
            harness: crate::profile::Harness::Claude,
            retry_until,
        });
}

/// Snapshot the queued switch targets in order, for asserting re-queue / clearing.
fn queued_targets(d: &Daemon) -> Vec<String> {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .iter()
        .map(|e| e.target.clone())
        .collect()
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
        }),
    }
}

/// A blank profile with a live-token credential block attached.
fn profile_with_creds(name: &str, access: &str) -> Profile {
    let mut p = blank_profile(name);
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
    crate::claude::force_link_profile_credentials(name).expect("link active credentials");
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
/// nothing else — the pure no-op characterization of one loop iteration.
#[test]
fn tick_with_empty_queues_writes_status_and_leaves_active_unchanged() {
    let _home = HomeSandbox::new();
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

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "an idle, clean, installable target must be switched to"
    );
}

/// A SCHEDULER switch is SKIPPED when the outgoing active has unsaved,
/// diverged credentials (a CC re-login / token rotation) — automation may not
/// discard a login clauth doesn't own, so it defers to the operator.
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

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a diverged, unsaved active must block a scheduler switch (no daemon prompt)"
    );
    // TECH-6: deferred, not dropped — the divergence may resolve, so it stays queued.
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "a switch blocked by divergence is re-queued for retry, not silently dropped"
    );
}

/// RESCUE-2: the same diverged state does NOT block a USER switch — the tap is
/// the operator decision the daemon can't prompt for. The unsaved live login is
/// archived into `~/.clauth/quarantine/` (loss-free) and the switch proceeds
/// with discard semantics. Pre-fix, a socket-originated user switch had no path
/// past a foreign live login and wedged until that login died (2026-07-16).
#[test]
fn drain_pending_switch_user_origin_archives_diverged_login_and_proceeds() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    diverge_active("at-alpha-FOREIGN");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "a user switch must proceed past a diverged live login"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "a landed switch must not be re-queued"
    );
    let quarantine = clauth_dir().expect("clauth dir").join("quarantine");
    let archived: Vec<_> = std::fs::read_dir(&quarantine)
        .expect("quarantine dir must exist after a discard switch")
        .map(|e| e.expect("dir entry").path())
        .collect();
    assert_eq!(
        archived.len(),
        1,
        "exactly one archived copy of the discarded login"
    );
    let name = archived[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.ends_with("-alpha.credentials.json"),
        "archive is named after the outgoing profile: {name}"
    );
    let saved = std::fs::read_to_string(&archived[0]).expect("read archived login");
    assert!(
        saved.contains("at-alpha-FOREIGN"),
        "the archived copy must hold the discarded login's tokens (loss-free)"
    );
    // The live slot now belongs to beta's stored chain.
    let live = crate::claude::read_claude_credentials()
        .expect("read live")
        .expect("live present");
    assert_eq!(
        live.access_token(),
        Some("at-beta"),
        "the live slot must hold the target's stored login after the discard switch"
    );
}

/// Claude Code's logged-out SHELL (both tokens blanked, `expiresAt: 0` — what
/// CC writes when its own refresh dies, keeping unrelated keys like
/// `mcpOAuth`) still classifies Diverged, but holds no login to protect. The
/// queued switch must PROCEED over it — even a headless (Scheduler) switch,
/// which otherwise defers on divergence — because deferring wedged every
/// headless switch behind a TUI decision about an empty file while running
/// sessions sat at "Login expired" (observed 2026-07-15).
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

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
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
    let alpha_store = crate::profile::profile_dir("alpha")
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

/// A live file that does not PARSE is not a shell — it may be a CC write in
/// progress, i.e. possibly a login. The divergence deferral stays armed for
/// it, exactly like a real diverged login: a headless (Scheduler) switch is
/// deferred and re-queued rather than proceeding over a possible login.
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

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
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

    stage_switch(&daemon, "ghost", Origin::User, now_ms() + 120_000);
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
    let stored = crate::profile::profile_dir("alpha")
        .unwrap()
        .join("credentials.json");
    assert!(stored.exists(), "alpha's stored credentials survive");
    assert!(
        daemon
            .last_error
            .as_ref()
            .is_some_and(|e| e.message.contains("ghost")),
        "the drop is observable in last_error, not silent"
    );
}

/// A switch to a target that is still mid-fetch can't execute this tick, but the
/// request is RE-QUEUED (not dropped after one attempt) and lands once the target
/// goes idle — the core TECH-6 fix (finding #4: a user tap during a fetch window
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
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    mark_activity(&daemon.activity, "beta", ProfileActivity::Fetching);
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

    // Fetch completes → the re-queued switch lands on the next tick.
    clear_activity(&daemon.activity, "beta");
    daemon.drain_pending_switch();
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "once idle, the re-queued switch executes"
    );
}

/// A user tap and a scheduler auto-target queued the same tick: the User request
/// wins the drain and the superseded scheduler target is dropped.
#[test]
fn user_switch_outranks_same_tick_auto() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("home", "at-home"),
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("home"),
        90_000,
    );
    link_active_clean("home");
    let mut daemon = daemon_for(config);

    // Stage both directly so they coexist at drain time (drain-side precedence).
    stage_switch(&daemon, "alpha", Origin::Scheduler, now_ms() + 120_000);
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "User origin outranks a same-tick Scheduler auto-switch"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "the superseded scheduler target is dropped, not left queued"
    );
}

/// The enqueue helper's clearing rule: a User request clears any queued Scheduler
/// target on the way in, so only the user's choice remains and lands.
#[test]
fn user_switch_clears_queued_scheduler() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("home", "at-home"),
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("home"),
        90_000,
    );
    link_active_clean("home");
    let mut daemon = daemon_for(config);

    {
        let mut q = daemon.pending_switch.lock().expect("pending_switch");
        enqueue_pending_switch(
            &mut q,
            "alpha".into(),
            crate::profile::Harness::Claude,
            Origin::Scheduler,
            now_ms(),
        );
        enqueue_pending_switch(
            &mut q,
            "beta".into(),
            crate::profile::Harness::Claude,
            Origin::User,
            now_ms(),
        );
    }
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "a user enqueue clears the queued scheduler target"
    );

    daemon.drain_pending_switch();
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "the user's choice is the one that lands"
    );
}

// ── drain_config_ops: fingerprint-suppression contract ────────────────────────

/// A threshold edit touches only the profile's `config.toml` and returns
/// `Ok(false)`, so `drain_config_ops` must NOT adopt a fresh `last_reload_fp` —
/// an external `profiles.toml` write that landed the same tick still triggers a
/// reload. Pins the exact regression the ledger caught by eyeball.
#[test]
fn drain_config_ops_threshold_does_not_suppress_external_reload() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let mut daemon = daemon_for(config);
    assert_eq!(daemon.refresh_interval.load(Ordering::Relaxed), 90_000);

    // Simulate an unrelated external edit landing this tick: rewrite profiles.toml
    // with a new refresh interval and force its mtime strictly ahead of the
    // daemon's recorded mtime.
    let external = AppState {
        active_profile: Some("alpha".into()),
        profiles: vec!["alpha".into()],
        refresh_interval_ms: 30_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external app-state write");
    let state_path = clauth_dir().unwrap().join("profiles.toml");
    set_mtime(&state_path, SystemTime::now() + Duration::from_secs(5));

    // A threshold edit (Ok(false)) must leave last_reload_fp untouched.
    daemon
        .pending_config_ops
        .lock()
        .expect("pending_config_ops")
        .push(ConfigOp::SetThreshold("alpha".into(), 50.0));
    daemon.drain_config_ops();

    // Because the threshold edit did not adopt a fresh fingerprint, the reload fires.
    daemon.reload_if_changed();
    assert_eq!(
        daemon.refresh_interval.load(Ordering::Relaxed),
        30_000,
        "a wrote_state=false edit must not swallow a same-tick external reload"
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

// ── TECH-7: cross-process RMW atomicity (lost-update) ─────────────────────────

/// The names in the on-disk `profiles.toml`, freshly reloaded from disk.
fn on_disk_profile_names() -> Vec<String> {
    load_config()
        .expect("reload config")
        .state
        .profiles
        .iter()
        .map(|n| n.to_string())
        .collect()
}

/// A daemon switch must PRESERVE a profile another process appended to
/// `profiles.toml` after the daemon loaded its config — the core lost-update fix
/// (finding #1). Without the reload-merge, `finish_switch`'s blind rewrite from the
/// daemon's stale snapshot would orphan the externally-added profile.
#[test]
fn lost_update_switch_preserves_externally_added_profile() {
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
    // Daemon's in-memory config is [alpha, beta] active=alpha.
    let mut daemon = daemon_for(config);

    // Simulate a concurrent `clauth login gamma`: gamma's dir+creds land on disk and
    // gamma is appended to profiles.toml — but the daemon still predates it (we do
    // NOT reload it, so its snapshot is stale, exactly the race window).
    save_profile(&profile_with_creds("gamma", "at-gamma")).expect("external profile dir");
    let external = AppState {
        active_profile: Some("alpha".into()),
        profiles: vec!["alpha".into(), "beta".into(), "gamma".into()],
        refresh_interval_ms: 90_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external profiles.toml append");

    // Daemon switches alpha→beta from its STALE snapshot (drain bypasses reload).
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    let names = on_disk_profile_names();
    assert!(
        names.iter().any(|n| n == "gamma"),
        "externally-added 'gamma' must survive the switch's merged save (got {names:?})"
    );
    assert!(
        names.iter().any(|n| n == "alpha") && names.iter().any(|n| n == "beta"),
        "the daemon's own profiles are still present"
    );
    assert_eq!(
        load_config().unwrap().state.active_profile.as_deref(),
        Some("beta"),
        "the switch itself still applied"
    );
}

/// After a switch, the daemon's `last_reload_fp` equals the on-disk fingerprint — it
/// adopted its OWN write (captured while holding the flock), so `reload_if_changed`
/// is a no-op for the self-write, yet a later external write (newer mtime) still
/// triggers a reload. This is the no-self-adoption-window contract (finding #1,
/// the :354 gap).
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

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
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

// ── TECH-8: switch-event observability + failure backoff ──────────────────────

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
/// engaged. This is the anti-log-storm contract (finding #38).
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

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    // beta never goes idle → every attempt fails with the same reason.
    mark_activity(&daemon.activity, "beta", ProfileActivity::Fetching);

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
            .get(&crate::profile::Harness::Claude)
            .is_some_and(|b| b.target == "beta" && b.attempts >= 3),
        "backoff engaged for the repeatedly-failing claude target"
    );
}

// ── TECH-9 #13: ~/.clauth 0700 enforcement ────────────────────────────────────

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

/// An executed switch records the `last_switch` hero event with from/to/trigger.
#[test]
fn successful_switch_records_last_switch_event() {
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

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    let ls = daemon.last_switch.as_ref().expect("last_switch recorded");
    assert_eq!(ls.from.as_deref(), Some("alpha"), "from = previous active");
    assert_eq!(ls.to.as_deref(), Some("beta"), "to = new active");
    assert_eq!(ls.trigger, "user", "trigger reflects the queue Origin");
}

/// RESCUE-1 test doubles: legs of `follow_live_login_with` a given case must
/// never reach. Panics beat silent misroutes.
fn no_refresh(
    _: &str,
    _: Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    panic!("the refresh probe must not run in this case")
}
fn no_gate(_: &str) -> crate::oauth::AuthGate {
    panic!("the install gate must not run in this case")
}

// ── follow_live_login: unattended sibling-divergence self-heal ──────────────
//
// When the ACTIVE profile's live link diverges and the live login PROVABLY
// belongs to a different stored profile, the daemon follows Claude Code there
// (capture + bookkeeping switch) instead of deferring forever with "resolve
// in the TUI". Identity fn injected — no network in tests.

/// Tier 1 (exact token match, no identity call): the live file carries the
/// sibling's exact stored pair — a half-landed switch. The daemon captures it
/// into the sibling and makes the sibling active; the identity fn must never
/// be called.
#[test]
fn follow_live_login_adopts_a_sibling_by_exact_token_match() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Live file = beta's EXACT stored credentials, while alpha is active.
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-beta")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );

    assert_eq!(
        active_of(&d).as_deref(),
        Some("beta"),
        "the daemon follows claude code to the owning profile"
    );
    // The persisted state agrees (narrow delta wrote through).
    let disk: AppState = toml::from_str(
        &std::fs::read_to_string(clauth_dir().expect("dir").join("profiles.toml"))
            .expect("read state"),
    )
    .expect("parse state");
    assert_eq!(disk.active_profile.as_deref(), Some("beta"));
}

/// Tier 2 (network-verified uuid vs the sibling's cached anchor): a fresh CC
/// re-login into a known account — every token new, identity proven by uuid.
/// The sibling's store adopts the live pair and becomes active.
#[test]
fn follow_live_login_adopts_a_sibling_by_verified_account_uuid() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        "beta",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-beta".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-fresh-relogin")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|tok| {
            assert_eq!(tok, "at-fresh-relogin");
            crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
                uuid: "uuid-beta".to_string(),
                email: None,
            })
        },
        &no_refresh,
        &no_gate,
    );

    assert_eq!(active_of(&d).as_deref(), Some("beta"));
    // The live pair was captured into beta's store.
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("beta")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read beta creds");
    assert_eq!(stored.access_token(), Some("at-fresh-relogin"));
}

/// A PROVEN-foreign login (an account clauth holds nowhere) is LEFT ALONE
/// (the TUI's decision) and memoized so the daemon doesn't re-examine (or
/// re-log) the same login every tick. Every stored login is anchored here —
/// the ForeignAccount verdict requires complete coverage (RESCUE-2b).
#[test]
fn follow_live_login_leaves_a_proven_foreign_login_alone_and_memoizes() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        "alpha",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    crate::profile_cache::write_profile_cache(
        "beta",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-beta".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-foreign")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
            uuid: "uuid-nobody-stores".to_string(),
            email: None,
        })
    };
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(active_of(&d).as_deref(), Some("alpha"), "nothing followed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Same login again: memo short-circuits — no second identity fetch.
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A NEW live login re-arms the examination.
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-foreign-2")).expect("ser"),
    )
    .expect("write live");
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A login matching the ACTIVE profile's own anchor is the adopt path's
/// domain: follow must stand down (and not spam the log).
#[test]
fn follow_live_login_stands_down_on_a_same_account_divergence() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        "alpha",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-alpha-rotated")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| {
            crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
                uuid: "uuid-alpha".to_string(),
                email: None,
            })
        },
        &no_refresh,
        &no_gate,
    );
    assert_eq!(
        active_of(&d).as_deref(),
        Some("alpha"),
        "same-account divergence is adopt's job, not follow's"
    );
}

/// RESCUE-2b: a PROVEN uuid that matches no anchor proves foreignness only
/// when every stored login HAS an anchor. With coverage incomplete (anchors
/// are dropped on unproven re-logins and backfilled by the next /profile
/// poll), the live login could still be an owned account — so it retries on
/// the timer instead of memoizing "resolve in the TUI" for good.
#[test]
fn follow_unmatched_proven_login_with_unanchored_profiles_retries_not_memoizes() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // alpha anchored; beta holds a login but has NO anchor → incomplete coverage.
    crate::profile_cache::write_profile_cache(
        "alpha",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-mystery")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
            uuid: "uuid-matches-no-anchor".to_string(),
            email: None,
        })
    };
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(active_of(&d).as_deref(), Some("alpha"), "nothing followed");
    assert_eq!(
        d.follow_memo, None,
        "incomplete anchor coverage must not memoize a possibly-owned login as foreign"
    );
    assert!(d.follow_retry_at > 0, "retries on the timer instead");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the window: quiet. Window elapsed: examined again.
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    d.follow_retry_at = 0;
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// RESCUE-2b: a sibling adoption whose local WRITE fails (here: a read-only
/// profile dir) arms the retry timer — it spent nothing, so it must not be
/// memoized against the login (the pre-RESCUE-2 behavior wedged a
/// legitimately owned login behind one transient local error for good).
#[cfg(unix)]
#[test]
fn follow_sibling_overwrite_failure_arms_the_retry_timer_not_the_memo() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Tier-1 exact-token match: the live login IS beta's stored chain
    // (a half-landed switch) — no network needed to attribute it.
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-beta")).expect("ser"),
    )
    .expect("write live");
    let beta_dir = crate::profile::profile_dir("beta").expect("beta dir");

    let mut d = daemon_for(config);
    std::fs::set_permissions(&beta_dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );
    std::fs::set_permissions(&beta_dir, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert_eq!(
        active_of(&d).as_deref(),
        Some("alpha"),
        "the failed adoption left the active profile unchanged"
    );
    assert_eq!(
        d.follow_memo, None,
        "a local write failure is never memoized against the login"
    );
    assert!(d.follow_retry_at > 0, "it retries on the timer");

    // Window elapsed + writable again: the adoption completes.
    d.follow_retry_at = 0;
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );
    assert_eq!(
        active_of(&d).as_deref(),
        Some("beta"),
        "the retry lands once the transient failure clears"
    );
}

// ── RESCUE-1: dead-live-login reclaim ────────────────────────────────────────
//
// A diverged live login the endpoint CONFIRMS dead protects nothing — the
// daemon reclaims the live slot with the active profile's stored chain instead
// of wedging every switch behind "resolve in the TUI" while the running
// `claude` stays signed out. Probes injected — no network in tests.

/// Writes a raw live-credentials JSON (regular file) and returns its path.
fn write_live_json(json: &serde_json::Value) -> std::path::PathBuf {
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(&live, serde_json::to_vec(json).expect("ser")).expect("write live");
    live
}

fn dead_probe(_: &str) -> crate::usage::IdentityProbe {
    crate::usage::IdentityProbe::Rejected
}

fn refresh_confirms_dead(
    _: &str,
    _: Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    Err(crate::oauth::RefreshError::Invalid(
        "HTTP 400: invalid_grant".to_string(),
    ))
}

fn gate_ready(_: &str) -> crate::oauth::AuthGate {
    crate::oauth::AuthGate::Ready
}

/// Endpoint-confirmed dead (identity 401 + refresh `invalid_grant`) + a healthy
/// stored chain → the live slot is reclaimed: the file becomes the active
/// profile's symlink again, and the memo/backoff clear so the next divergence
/// is examined fresh.
#[test]
fn rescue_reclaims_an_endpoint_confirmed_dead_live_login() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_ready);

    let meta = live.symlink_metadata().expect("live exists");
    assert!(
        meta.file_type().is_symlink(),
        "the corpse is replaced by the active profile's link"
    );
    let target = std::fs::read_link(&live).expect("readlink");
    assert!(
        target.ends_with("alpha/credentials.json"),
        "relinked to the ACTIVE profile's stored chain: {}",
        target.display()
    );
    assert_eq!(active_of(&d).as_deref(), Some("alpha"), "active unchanged");
    assert_eq!(d.follow_memo, None, "no memo left against the dead login");
    assert_eq!(
        d.follow_retry_at, 0,
        "no backoff left after a completed rescue"
    );
}

/// `AuthGate::Refreshed` (the stored chain was expiring and the gate rotated
/// it on the way in) shares the reclaim's pass-arm with `Ready` — the freshly
/// rotated chain must still take the slot. Pins the coalescing: sibling call
/// sites already treat Refreshed distinctly (the TUI's switch path), so a
/// future split of this match must not silently drop the reclaim.
#[test]
fn rescue_reclaims_through_a_refreshed_install_gate() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let gate_refreshed = |_: &str| crate::oauth::AuthGate::Refreshed;
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_refreshed);

    assert!(
        live.symlink_metadata()
            .expect("live exists")
            .file_type()
            .is_symlink(),
        "a Refreshed gate must reclaim exactly like Ready"
    );
    assert_eq!(d.follow_memo, None);
    assert_eq!(d.follow_retry_at, 0);
}

/// The identity endpoint rejected the access token but the refresh leg only
/// failed transiently: proves nothing — no reclaim, and the network tier backs
/// off on the timer instead of re-probing every tick (or memoizing for good).
#[test]
fn rescue_backs_off_when_the_refresh_leg_is_transient() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Rejected
    };
    let transient =
        |_: &str,
         _: Option<&str>|
         -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
            Err(crate::oauth::RefreshError::Transient(anyhow::anyhow!(
                "connection reset"
            )))
        };
    d.follow_live_login_with(&identity, &transient, &no_gate);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "no reclaim on an unconfirmed death"
    );
    assert!(d.follow_retry_at > 0, "a retry window is armed");
    assert_eq!(d.follow_memo, None, "a transient outcome is never memoized");

    // Inside the window: the network tier stays quiet (tier 1 still runs).
    d.follow_live_login_with(&identity, &transient, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Window elapsed: the probe re-runs.
    d.follow_retry_at = 0;
    d.follow_live_login_with(&identity, &transient, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// The refresh probe SUCCEEDED — the pair was alive (the identity 401 was the
/// access token dying of old age). The probe consumed the file's single-use
/// refresh token, so the fresh pair lands straight back in the live file —
/// preserving every foreign top-level key (CC parks `mcpOAuth` there) — and
/// nothing is reclaimed.
#[test]
fn rescue_writes_a_still_alive_pair_back_in_place() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "at-stale",
            "refreshToken": "rt-stale",
            "expiresAt": 0,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        },
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    }));

    let mut d = daemon_for(config);
    let refresh_ok =
        |rt: &str,
         scopes: Option<&str>|
         -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
            assert_eq!(rt, "rt-stale", "must spend the live file's refresh token");
            assert_eq!(scopes, Some("user:inference"), "must carry the live scopes");
            Ok(crate::oauth::TokenResponse {
                access_token: "at-rotated".to_string(),
                refresh_token: "rt-rotated".to_string(),
                expires_in: 28_800,
                scope: None,
            })
        };
    d.follow_live_login_with(&dead_probe, &refresh_ok, &no_gate);

    let meta = live.symlink_metadata().expect("live");
    assert!(
        !meta.file_type().is_symlink(),
        "an alive login is never reclaimed"
    );
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live).expect("read")).expect("json");
    assert_eq!(v["claudeAiOauth"]["accessToken"], "at-rotated");
    assert_eq!(v["claudeAiOauth"]["refreshToken"], "rt-rotated");
    assert!(
        v["claudeAiOauth"]["expiresAt"].as_u64().expect("ms") > crate::usage::now_ms(),
        "expiry re-stamped in the future"
    );
    assert_eq!(
        v["claudeAiOauth"]["subscriptionType"], "max",
        "untouched oauth fields survive"
    );
    assert_eq!(
        v["mcpOAuth"]["some-server"]["accessToken"], "mcp-tok",
        "foreign top-level keys survive the surgical write"
    );
    assert!(
        d.follow_retry_at > 0,
        "the follow-up re-identification waits out the probe window — an instant \
         retry would let a pathologically still-401ing token refresh-storm"
    );
}

/// Confirmed-dead live login but the active's STORED chain is broken too:
/// nothing installable may take the slot (AUTH-1) — no reclaim, retry armed.
#[test]
fn rescue_refuses_when_the_stored_chain_is_broken_too() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let gate_broken = |_: &str| crate::oauth::AuthGate::Broken;
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_broken);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "a dead stored chain must never take the live slot"
    );
    assert!(d.follow_retry_at > 0, "retries once a re-login lands");
}

/// A live login whose access token is rejected and which carries NO refresh
/// token is unusable by anyone — reclaimed directly, no refresh leg to consult.
#[test]
fn rescue_reclaims_a_refreshless_rejected_login_directly() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "at-dead", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(&dead_probe, &no_refresh, &gate_ready);

    assert!(
        live.symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "nothing to preserve, nothing to confirm — reclaimed"
    );
}

/// Concurrent-write guard: fresh credentials landing between the probe and the
/// reclaim (a CC-side re-login or refresh) must never be overwritten. The
/// refresh leg simulates the race by rewriting the live file before returning
/// its confirmed-dead verdict.
#[test]
fn rescue_aborts_when_the_live_login_changes_mid_probe() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let live_for_closure = live.clone();
    let racing_refresh = move |_: &str,
                               _: Option<&str>|
          -> std::result::Result<
        crate::oauth::TokenResponse,
        crate::oauth::RefreshError,
    > {
        // A concurrent CC re-login replaces the file mid-probe.
        std::fs::write(
            &live_for_closure,
            serde_json::to_vec(&oauth_creds("at-brand-new-login")).expect("ser"),
        )
        .expect("racing write");
        Err(crate::oauth::RefreshError::Invalid(
            "HTTP 400: invalid_grant".to_string(),
        ))
    };
    d.follow_live_login_with(&dead_probe, &racing_refresh, &gate_ready);

    let meta = live.symlink_metadata().expect("live");
    assert!(
        !meta.file_type().is_symlink(),
        "the freshly landed login must survive the aborted reclaim"
    );
    let survived: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read survived login");
    assert_eq!(
        survived.access_token(),
        Some("at-brand-new-login"),
        "the racing login's bytes are untouched"
    );
}

/// A probe OUTAGE (Indeterminate — the 2026-07-14 incident class) must never
/// reclaim, never memoize, and retry on the timer. This is the central
/// RESCUE-1 guarantee: memoizing one bad probe against the login is what
/// wedged the daemon for a day.
#[test]
fn follow_probe_outage_is_never_memoized_and_retries_on_the_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-mystery")).expect("val"));

    let mut d = daemon_for(config);
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let outage = |_: &str| {
        probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Indeterminate
    };
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "an unproven login is never reclaimed"
    );
    assert_eq!(
        d.follow_memo, None,
        "a probe outage is NEVER memoized against the login"
    );
    assert!(d.follow_retry_at > 0, "the retry window is armed");
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the window: quiet (tier 1 only).
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Window elapsed: probed again — the outage was not terminal.
    d.follow_retry_at = 0;
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// Write-back leg of the mid-probe race: the refresh SUCCEEDS but a fresh CC
/// login landed during the roundtrip. The rotated pair must be discarded (it
/// continues the corpse's superseded lineage) — writing it would destroy the
/// fresh login's only refresh-token copy.
#[test]
fn rescue_write_back_aborts_when_a_fresh_login_lands_mid_refresh() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let live_for_closure = live.clone();
    let racing_refresh = move |_: &str,
                               _: Option<&str>|
          -> std::result::Result<
        crate::oauth::TokenResponse,
        crate::oauth::RefreshError,
    > {
        std::fs::write(
            &live_for_closure,
            serde_json::to_vec(&oauth_creds("at-brand-new-login")).expect("ser"),
        )
        .expect("racing write");
        Ok(crate::oauth::TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 28_800,
            scope: None,
        })
    };
    d.follow_live_login_with(&dead_probe, &racing_refresh, &no_gate);

    let survived: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read survived login");
    assert_eq!(
        survived.access_token(),
        Some("at-brand-new-login"),
        "the freshly landed login must never be clobbered by the discarded rotation"
    );
}

/// Reclaim FAILURE (gate passed, fingerprint matched, but the relink itself
/// errored): arms the retry timer — never memoizes, never wedges.
#[cfg(unix)]
#[test]
fn rescue_reclaim_failure_arms_the_retry_timer() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));
    let claude = live.parent().expect("claude dir").to_path_buf();

    let mut d = daemon_for(config);
    // Read-only ~/.claude: force_link's remove_file fails after every gate passed.
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_ready);
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "the failed reclaim left the corpse in place"
    );
    assert!(
        d.follow_retry_at > 0,
        "a failed reclaim retries on the timer"
    );
    assert_eq!(d.follow_memo, None, "a failed reclaim is never memoized");
}

/// Write-back FAILURE (refresh succeeded — the single-use refresh token is
/// spent — but the rotated pair could not be persisted): the live chain is
/// lost through our own probe. Memoized + named loudly; only a re-login
/// recovers, so retrying would just spend more tokens.
#[cfg(unix)]
#[test]
fn rescue_write_back_failure_memoizes_the_lost_chain() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));
    let claude = live.parent().expect("claude dir").to_path_buf();

    let mut d = daemon_for(config);
    let claude_for_closure = claude.clone();
    // The dir turns read-only mid-rescue: the fingerprint re-read still works
    // (read allowed) but the atomic write of the rotated pair cannot land.
    let refresh_then_lock = move |_: &str,
                                  _: Option<&str>|
          -> std::result::Result<
        crate::oauth::TokenResponse,
        crate::oauth::RefreshError,
    > {
        std::fs::set_permissions(&claude_for_closure, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        Ok(crate::oauth::TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 28_800,
            scope: None,
        })
    };
    d.follow_live_login_with(&dead_probe, &refresh_then_lock, &no_gate);
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        d.follow_memo.is_some(),
        "a lost chain is memoized — only a re-login recovers, retrying spends more tokens"
    );
}

/// Stored chain refreshes transiently mid-reclaim (gate Transient): no reclaim,
/// retry armed — a network blip on OUR side must not destroy the corpse early.
#[test]
fn rescue_gate_transient_arms_the_retry_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let gate_transient = |_: &str| crate::oauth::AuthGate::Transient(anyhow::anyhow!("blip"));
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_transient);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "no reclaim behind a transient gate"
    );
    assert!(d.follow_retry_at > 0);
    assert_eq!(d.follow_memo, None);
}

/// A Proven identity with a BLANK uuid is shape drift, not an identity: it must
/// read as unproven (timed retry), never as a proven-foreign account (memo).
#[test]
fn follow_blank_uuid_reads_as_unproven_not_foreign() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    write_live_json(&serde_json::to_value(oauth_creds("at-blank")).expect("val"));

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| {
            crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
                uuid: "   ".to_string(),
                email: None,
            })
        },
        &no_refresh,
        &no_gate,
    );
    assert_eq!(
        d.follow_memo, None,
        "two blanks must never prove an identity"
    );
    assert!(d.follow_retry_at > 0, "unproven retries on the timer");
}

/// A foreign login observed while the probe window is closed reads as unproven
/// — the identity fn must not run, and nothing is memoized (the real verdict
/// waits for the window).
#[test]
fn follow_foreign_login_inside_backoff_is_not_memoized() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    write_live_json(&serde_json::to_value(oauth_creds("at-foreign")).expect("val"));

    let mut d = daemon_for(config);
    d.follow_retry_at = crate::usage::now_ms() + 600_000;
    d.follow_live_login_with(
        &|_| panic!("the identity probe must not run inside the backoff window"),
        &no_refresh,
        &no_gate,
    );
    assert_eq!(d.follow_memo, None, "no verdict, no memo");
}

/// RESCUE-1b: Claude Code's logged-out SHELL (blanked tokens, expiresAt 0 —
/// what CC writes when its own refresh dies) is not a login at all. It must be
/// reclaimed on sight instead of wedging switches behind a TUI decision about
/// nothing — the exact live state observed on 2026-07-15.
#[test]
fn follow_reclaims_a_logged_out_live_shell() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "",
            "refreshToken": "",
            "expiresAt": 0,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        },
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("a token-less shell has nothing to probe"),
        &no_refresh,
        &gate_ready,
    );

    let meta = live.symlink_metadata().expect("live");
    assert!(
        meta.file_type().is_symlink(),
        "the shell is replaced by the active profile's link"
    );
    let target = std::fs::read_link(&live).expect("readlink");
    assert!(target.ends_with("alpha/credentials.json"));
    assert_eq!(d.follow_memo, None);
    assert_eq!(d.follow_retry_at, 0);
}

/// A file with NO OAuth block at all (only foreign keys like mcpOAuth) is the
/// same shell — reclaimed, not deferred.
#[test]
fn follow_reclaims_a_live_file_with_no_oauth_block() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(&|_| panic!("no token to probe"), &no_refresh, &gate_ready);
    assert!(
        live.symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "an OAuth-less live file is a shell — reclaimed"
    );
}

/// Shell + a broken stored chain: nothing installable may take the slot
/// (AUTH-1). No reclaim, and the retry timer keeps the install gate from
/// re-running every tick.
#[test]
fn follow_logged_out_shell_with_broken_store_waits_on_the_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    let gates = std::sync::atomic::AtomicUsize::new(0);
    let gate_broken = |_: &str| {
        gates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Broken
    };
    d.follow_live_login_with(&|_| panic!("nothing to probe"), &no_refresh, &gate_broken);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "a dead stored chain must never take the live slot"
    );
    assert!(d.follow_retry_at > 0);
    assert_eq!(gates.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the window the gate is not re-run.
    d.follow_live_login_with(&|_| panic!("nothing to probe"), &no_refresh, &gate_broken);
    assert_eq!(gates.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Shell race: a REAL login lands (CC-side /login) between the shell judgment
/// and the relink — the late still-unchanged re-check must abort the reclaim.
#[test]
fn follow_logged_out_shell_race_aborts_the_reclaim() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    let live_for_closure = live.clone();
    let racing_gate = move |_: &str| {
        // A concurrent CC /login lands a fresh pair mid-reclaim.
        std::fs::write(
            &live_for_closure,
            serde_json::to_vec(&oauth_creds("at-fresh-cc-login")).expect("ser"),
        )
        .expect("racing write");
        crate::oauth::AuthGate::Ready
    };
    d.follow_live_login_with(&|_| panic!("nothing to probe"), &no_refresh, &racing_gate);

    let survived: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read survived login");
    assert_eq!(
        survived.access_token(),
        Some("at-fresh-cc-login"),
        "the freshly landed login must survive the aborted shell reclaim"
    );
}

/// RESCUE-2b mixed state: blank access token but the live refresh token
/// byte-matches the ACTIVE profile's own stored chain — a degraded copy of the
/// same chain (torn write). Relinking loses nothing: reclaimed like a shell.
#[test]
fn follow_reclaims_a_degraded_copy_of_the_active_chain() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    // oauth_creds seeds alpha's stored refresh token as "rt-at-alpha".
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "rt-at-alpha", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("a token-less file has nothing to probe"),
        &no_refresh,
        &gate_ready,
    );

    assert!(
        live.symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "a degraded copy of the active profile's own chain is reclaimed"
    );
    assert_eq!(d.follow_memo, None);
    assert_eq!(d.follow_retry_at, 0);
}

/// RESCUE-2b mixed state, foreign flavor: blank access token but an
/// UNRECOGNIZED refresh token. There may be a recoverable login in it, so it
/// is left alone — but visibly, on the retry timer, instead of the silent
/// per-tick no-op this state used to be.
#[test]
fn follow_leaves_an_unrecognized_refresh_only_file_alone_on_the_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "rt-nobody-stores", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    let gates = std::sync::atomic::AtomicUsize::new(0);
    let counting_gate = |_: &str| {
        gates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Ready
    };
    d.follow_live_login_with(
        &|_| panic!("no access token to probe"),
        &no_refresh,
        &counting_gate,
    );

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "an unrecognized refresh token is never clobbered"
    );
    assert_eq!(
        gates.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no reclaim attempted — the gate is never consulted"
    );
    assert_eq!(d.follow_memo, None, "not memoized (the state may resolve)");
    assert!(
        d.follow_retry_at > 0,
        "re-examined on the timer, not per tick"
    );

    // Inside the window: fully quiet.
    d.follow_live_login_with(&|_| panic!("gated"), &no_refresh, &counting_gate);
    assert_eq!(gates.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// ── RESCUE-2b: follow state survives a daemon restart ────────────────────────

/// The 30-min network backoff is persisted: a daemon restart (launchd respawn,
/// `pkill` deploy) inside the window must NOT re-arm the network tier — that
/// was how a restart loop could re-probe (or re-spend a single-use refresh
/// token) once per boot.
#[test]
fn follow_retry_backoff_survives_a_daemon_restart() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-mystery")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let outage = |_: &str| crate::usage::IdentityProbe::Indeterminate;
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);
    let armed = d.follow_retry_at;
    assert!(armed > now_ms(), "the outage armed the backoff");

    // "Restart": a fresh Daemon over the same sandbox.
    let config2 = crate::profile::load_config().expect("reload config");
    let mut d2 = daemon_for(config2);
    assert_eq!(
        d2.follow_retry_at, armed,
        "the armed backoff survives the restart"
    );
    d2.follow_live_login_with(
        &|_| panic!("the network tier must stay gated across a restart"),
        &no_refresh,
        &no_gate,
    );
}

/// The proven-foreign memo is persisted too: a restart must not re-probe (and
/// re-log "resolve in the TUI" for) the same foreign login on every boot.
#[test]
fn follow_foreign_memo_survives_a_daemon_restart() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        "alpha",
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-foreign")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
            uuid: "uuid-nobody-stores".to_string(),
            email: None,
        })
    };
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(d.follow_memo.is_some(), "proven-foreign memoized");

    let config2 = crate::profile::load_config().expect("reload config");
    let mut d2 = daemon_for(config2);
    assert_eq!(
        d2.follow_memo, d.follow_memo,
        "the memo survives the restart"
    );
    d2.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the same foreign login is not re-probed after a restart"
    );
}

// ── CAP-1 tripwire: duplicate stored logins are named ────────────────────────

/// Two profiles storing byte-identical access tokens = one was captured over
/// with the other's chain (they now double-poll ONE account). The pure pair
/// detector is the daemon's per-tick tripwire; the warning memoizes on the
/// pair-set fingerprint so it logs once per distinct state, not per tick.
#[test]
fn duplicate_stored_logins_are_paired() {
    let _home = HomeSandbox::new();
    let with_token = |name: &str, tok: &str| {
        let mut p = blank_profile(name);
        p.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: tok.to_string(),
                refresh_token: Some(format!("{tok}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        p
    };
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            with_token("a", "shared-token"),
            with_token("b", "unique-token"),
            with_token("c", "shared-token"),
            blank_profile("no-creds"),
        ],
    };

    assert_eq!(
        super::tick::duplicate_login_pairs(&config),
        vec![("a".to_string(), "c".to_string())],
        "the first holder is named alongside each duplicate",
    );

    let clean = AppConfig {
        state: AppState::default(),
        profiles: vec![with_token("a", "t1"), with_token("b", "t2")],
    };
    assert!(
        super::tick::duplicate_login_pairs(&clean).is_empty(),
        "distinct chains raise nothing",
    );
}

/// CAP-2 tripwire: two profiles anchored to the same ACCOUNT under DIFFERENT
/// tokens (a re-login minted the wrong account — the 2026-07-12 recurrence)
/// double-poll it exactly like a copied chain, but the byte-identical check is
/// blind to it. The anchor-pair detector names it.
#[test]
fn duplicate_account_anchors_are_paired() {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, write_profile_cache};
    let _home = HomeSandbox::new();
    let names: Vec<String> = ["a", "b", "c", "blank-1", "blank-2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    write_profile_cache("a", ACCOUNT_ID_CACHE_FILE, &"acct-1".to_string());
    write_profile_cache("b", ACCOUNT_ID_CACHE_FILE, &"acct-2".to_string());
    write_profile_cache("c", ACCOUNT_ID_CACHE_FILE, &"acct-1".to_string());
    // TWO whitespace-only anchors: shape drift, not identities — absent the
    // is_empty guard they would trim equal and pair, so this fixture is what
    // actually locks the guard in (same contract as fetch_account_uuid).
    write_profile_cache("blank-1", ACCOUNT_ID_CACHE_FILE, &"  ".to_string());
    write_profile_cache("blank-2", ACCOUNT_ID_CACHE_FILE, &" ".to_string());

    assert_eq!(
        super::tick::duplicate_account_pairs(&names),
        vec![("a".to_string(), "c".to_string())],
        "the first holder is named alongside each same-account duplicate; blanks never pair",
    );

    write_profile_cache("c", ACCOUNT_ID_CACHE_FILE, &"acct-3".to_string());
    assert!(
        super::tick::duplicate_account_pairs(&names[..3]).is_empty(),
        "distinct accounts raise nothing",
    );
}

/// A byte-identical pair is ALSO anchor-identical once the backfill runs — the
/// warn path must report it once (under the sharper token message), never
/// twice. Locks in the `account_only_pairs` filter the commit message promises.
#[test]
fn token_pairs_are_filtered_from_the_account_report() {
    let token_pairs = vec![("a".to_string(), "c".to_string())];
    let account_pairs = vec![
        ("a".to_string(), "c".to_string()), // copied chain: in BOTH detectors
        ("d".to_string(), "e".to_string()), // wrong-account re-login: anchors only
    ];
    assert_eq!(
        super::tick::account_only_pairs(&token_pairs, account_pairs),
        vec![("d".to_string(), "e".to_string())],
        "the token-identical pair reports once, under the token message",
    );
    assert_eq!(
        super::tick::account_only_pairs(&[], vec![("a".into(), "c".into())]),
        vec![("a".to_string(), "c".to_string())],
        "no token pairs → account pairs pass through",
    );
}

// ---- CDX-1 T6: codex drain + follow ----

fn codex_auth_bytes(access: &str, account_id: &str) -> Vec<u8> {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": access,
            "refresh_token": format!("rt-{access}"),
            "account_id": account_id,
        },
    })
    .to_string()
    .into_bytes()
}

/// Persist a codex profile (harness marker + stored auth bytes) into the
/// sandbox and return its in-memory Profile.
fn codex_profile(name: &str, access: &str, account_id: &str) -> Profile {
    let mut p = blank_profile(name);
    p.harness = crate::profile::Harness::Codex;
    save_profile(&p).expect("persist codex profile");
    crate::codex::write_profile_auth(name, &codex_auth_bytes(access, account_id))
        .expect("persist codex auth");
    p
}

fn codex_active_of(d: &Daemon) -> Option<String> {
    d.config
        .lock()
        .expect("config")
        .state
        .active_codex_profile
        .as_deref()
        .map(str::to_string)
}

// A socket-origin (User) codex switch over a FOREIGN live login archives it to
// quarantine and proceeds — the codex mirror of the RESCUE-2 claude test above.
// The claude active slot must be untouched throughout.
#[test]
fn drain_codex_switch_user_origin_archives_foreign_and_proceeds() {
    let _home = HomeSandbox::new();
    let mut config = persist(
        vec![profile_with_creds("claude-a", "at-claude")],
        Some("claude-a"),
        90_000,
    );
    link_active_clean("claude-a");
    config
        .profiles
        .push(codex_profile("cdx-a", "at-alpha", "acct-alpha"));
    config
        .profiles
        .push(codex_profile("cdx-b", "at-beta", "acct-beta"));
    config.state.profiles.push("cdx-a".into());
    config.state.profiles.push("cdx-b".into());
    save_app_state(&config.state).expect("persist state");
    crate::codex::write_live(&codex_auth_bytes("at-FOREIGN", "acct-foreign")).unwrap();
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "cdx-a", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(codex_active_of(&daemon).as_deref(), Some("cdx-a"));
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("claude-a"),
        "the claude slot must be untouched by a codex switch"
    );
    let live = crate::codex::read_live().unwrap().expect("live");
    assert!(String::from_utf8_lossy(&live).contains("at-alpha"));
    let quarantine = clauth_dir().expect("clauth dir").join("quarantine");
    let archived: Vec<_> = std::fs::read_dir(&quarantine)
        .expect("quarantine dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.to_string_lossy().ends_with(".codex-auth.json"))
        .collect();
    assert_eq!(archived.len(), 1, "one archived copy of the foreign login");
    let saved = std::fs::read_to_string(&archived[0]).unwrap();
    assert!(saved.contains("at-FOREIGN"), "loss-free");
    assert!(queued_targets(&daemon).is_empty());
}

// A scheduler-origin codex switch defers on a foreign live login (retries via
// the shared backoff) — automation never displaces a login clauth doesn't own.
#[test]
fn drain_codex_switch_scheduler_origin_defers_on_foreign() {
    let _home = HomeSandbox::new();
    let mut config = persist(vec![], None, 90_000);
    config
        .profiles
        .push(codex_profile("cdx-a", "at-alpha", "acct-alpha"));
    config.state.profiles.push("cdx-a".into());
    save_app_state(&config.state).expect("persist state");
    let foreign = codex_auth_bytes("at-FOREIGN", "acct-foreign");
    crate::codex::write_live(&foreign).unwrap();
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "cdx-a", Origin::Scheduler, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(codex_active_of(&daemon), None, "switch must not land");
    assert_eq!(
        crate::codex::read_live().unwrap().as_deref(),
        Some(&foreign[..]),
        "the foreign live login is left alone"
    );
    assert_eq!(
        queued_targets(&daemon),
        vec!["cdx-a".to_string()],
        "the deferred switch is re-queued for retry"
    );
}

// The codex follow adopts a rotated live chain back into its owner's store
// (live→store only — the live file itself is never written by the follow).
#[test]
fn codex_follow_adopts_a_rotated_live_chain() {
    let _home = HomeSandbox::new();
    let mut config = persist(vec![], None, 90_000);
    config
        .profiles
        .push(codex_profile("cdx-a", "at-alpha", "acct-alpha"));
    config.state.profiles.push("cdx-a".into());
    config.state.active_codex_profile = Some("cdx-a".into());
    save_app_state(&config.state).expect("persist state");
    let rotated = codex_auth_bytes("at-alpha-ROTATED", "acct-alpha");
    crate::codex::write_live(&rotated).unwrap();
    let mut daemon = daemon_for(config);

    daemon.tick();

    assert_eq!(
        crate::codex::read_profile_auth("cdx-a").unwrap().as_deref(),
        Some(&rotated[..]),
        "the rotation was adopted back into the store"
    );
    assert_eq!(
        crate::codex::read_live().unwrap().as_deref(),
        Some(&rotated[..]),
        "the live file is never written by the follow"
    );
    assert_eq!(codex_active_of(&daemon).as_deref(), Some("cdx-a"));
}

// When the live login belongs to a DIFFERENT stored profile (the user ran
// `codex login` or swapped by hand), the follow syncs the active marker to
// the real owner instead of leaving status stale.
#[test]
fn codex_follow_syncs_active_to_the_live_owner() {
    let _home = HomeSandbox::new();
    let mut config = persist(vec![], None, 90_000);
    config
        .profiles
        .push(codex_profile("cdx-a", "at-alpha", "acct-alpha"));
    config
        .profiles
        .push(codex_profile("cdx-b", "at-beta", "acct-beta"));
    config.state.profiles.push("cdx-a".into());
    config.state.profiles.push("cdx-b".into());
    config.state.active_codex_profile = Some("cdx-a".into());
    save_app_state(&config.state).expect("persist state");
    crate::codex::write_live(&codex_auth_bytes("at-beta", "acct-beta")).unwrap();
    let mut daemon = daemon_for(config);

    daemon.tick();

    assert_eq!(
        codex_active_of(&daemon).as_deref(),
        Some("cdx-b"),
        "the marker follows the live owner"
    );
    // Persisted too, not just in-memory.
    let on_disk = crate::profile::load_config().expect("reload");
    assert_eq!(on_disk.state.active_codex_profile.as_deref(), Some("cdx-b"));
}

// A foreign live login is left alone and logged ONCE (memoized per distinct
// login) — and the memo re-arms when the live login changes.
#[test]
fn codex_follow_leaves_a_foreign_login_alone_and_memoizes() {
    let _home = HomeSandbox::new();
    let mut config = persist(vec![], None, 90_000);
    config
        .profiles
        .push(codex_profile("cdx-a", "at-alpha", "acct-alpha"));
    config.state.profiles.push("cdx-a".into());
    config.state.active_codex_profile = Some("cdx-a".into());
    save_app_state(&config.state).expect("persist state");
    let foreign = codex_auth_bytes("at-FOREIGN", "acct-foreign");
    crate::codex::write_live(&foreign).unwrap();
    let mut daemon = daemon_for(config);

    daemon.tick();
    let memo_after_first = daemon.codex_follow_memo;
    assert!(memo_after_first.is_some(), "foreign login memoized");
    daemon.tick();
    assert_eq!(
        daemon.codex_follow_memo, memo_after_first,
        "same login → same memo (no re-log)"
    );
    assert_eq!(
        crate::codex::read_live().unwrap().as_deref(),
        Some(&foreign[..]),
        "foreign login untouched"
    );
    assert_eq!(
        codex_active_of(&daemon).as_deref(),
        Some("cdx-a"),
        "the marker is not torn down for a foreign login"
    );

    // A different foreign login re-arms the memo (new fingerprint).
    crate::codex::write_live(&codex_auth_bytes("at-FOREIGN-2", "acct-foreign2")).unwrap();
    daemon.tick();
    assert_ne!(daemon.codex_follow_memo, memo_after_first);
}

// The noninteractive (MCP) entry point dispatches by harness: a codex target
// switches the codex slot and reports the codex previous/active pair.
#[test]
fn noninteractive_switch_dispatches_codex_targets() {
    let _home = HomeSandbox::new();
    let mut config = persist(vec![], None, 90_000);
    config
        .profiles
        .push(codex_profile("cdx-a", "at-alpha", "acct-alpha"));
    config
        .profiles
        .push(codex_profile("cdx-b", "at-beta", "acct-beta"));
    config.state.profiles.push("cdx-a".into());
    config.state.profiles.push("cdx-b".into());
    config.state.active_codex_profile = Some("cdx-b".into());
    save_app_state(&config.state).expect("persist state");
    crate::codex::write_live(&codex_auth_bytes("at-beta", "acct-beta")).unwrap();

    let handle = std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
    let (previous, active) =
        crate::actions::switch_profile_noninteractive(&handle, "cdx-a", None, |_, _| {
            unreachable!("a codex switch must never hit the OAuth refresher")
        })
        .expect("switch");
    assert_eq!(previous.as_deref(), Some("cdx-b"));
    assert_eq!(active, "cdx-a");
    let live = crate::codex::read_live().unwrap().expect("live");
    assert!(String::from_utf8_lossy(&live).contains("at-alpha"));
}

// ── Upstream v0.13 tests (daemon singleton modes, disable, #58 final) ──────

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
        }),
    };
    let alpha_dir = crate::profile::profile_dir("alpha").expect("alpha dir");
    std::fs::write(
        alpha_dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("serialize sidecar"),
    )
    .expect("write session-token sidecar");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
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
        }),
    };
    let alpha_dir = crate::profile::profile_dir("alpha").expect("alpha dir");
    std::fs::write(
        alpha_dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("serialize sidecar"),
    )
    .expect("write session-token sidecar");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
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
    daemon.switch_backoff.insert(
        crate::profile::Harness::Claude,
        super::SwitchBackoff {
            target: "beta".into(),
            attempts: 9,
            // Capped backoff step reaches PAST the retry window's edge.
            not_before: now + 60_000,
            reason: "target is mid-fetch".into(),
        },
    );
    // Fork model: the give-up TTL lives on the QUEUE ENTRY, not the backoff.
    stage_switch(&daemon, "beta", Origin::Scheduler, now.saturating_sub(1));

    daemon.drain_pending_switch();

    assert!(
        !daemon
            .switch_backoff
            .contains_key(&crate::profile::Harness::Claude),
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
    let mut disabled = blank_profile("off");
    disabled.max_auto_spend = Some(5.0);
    disabled.disabled = true;
    let mut enabled = blank_profile("on");
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

    super::serve(super::StartMode::ExitIfRunning).expect("a redundant instance exits clean");

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
