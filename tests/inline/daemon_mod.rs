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
use crate::testutil::{HomeSandbox, blank_profile, set_mtime};
use crate::usage::{ProfileActivity, clear_activity, mark_activity, now_ms};

use super::Daemon;

/// Queue a switch target on the daemon's pending set.
fn stage_switch(d: &Daemon, target: &str) {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .insert(target.into());
}

/// Snapshot the queued switch targets (sorted), for asserting re-queue / clearing.
fn queued_targets(d: &Daemon) -> Vec<String> {
    let mut v: Vec<String> = d
        .pending_switch
        .lock()
        .expect("pending_switch")
        .iter()
        .cloned()
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

    stage_switch(&daemon, "beta");
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "an idle, clean, installable target must be switched to"
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
        }),
    };
    let alpha_dir = crate::profile::profile_dir("alpha").expect("alpha dir");
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
    let stored = crate::profile::profile_dir("alpha")
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

    super::serve(super::StartMode::ExitIfRunning, None).expect("a redundant instance exits clean");

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
