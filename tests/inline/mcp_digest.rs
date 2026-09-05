#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! The since-your-last-call digest, pinned on the traps that make it a
//! feature rather than a no-op:
//!
//! - the baseline is SHARED across server clones (rmcp clones the handler per
//!   request; a per-clone baseline reports nothing forever);
//! - a first call reports nothing (there was no earlier state to compare
//!   against, and claiming "nothing changed" would assert otherwise);
//! - reporting consumes the delta, and a surface that does not report must not
//!   swallow it (the all-scope roster);
//! - `switch_profile` never reports its own write (it reseeds), but an arm
//!   that refused before any mutation reports like the session-scope roster
//!   does;
//! - `monitor` with no `job_ids` returns as soon as something moves, never
//!   sleeps holding the baseline lock;
//! - the usage cache is keyed on the profile it was read from, so a profile
//!   change is never dressed up as a refresh of a file nobody refreshed;
//! - a batch is one call: one digest, top-level, rendered in the prose reply,
//!   and a background `delegate` handle is a batch of one or of N under the
//!   same rule.
//!
//! Replies are prose now (the JSON payload is internal to the renderers), so
//! the digest assertions read the prose clause: "since your last call: ...".

use super::*;
use crate::profile::{AppState, ProfileName, save_app_state};
use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE};
use crate::testutil::{HomeSandbox, set_mtime};
use crate::usage::UsageInfo;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A fixed old stamp and its successor: distinct values, so a `set_mtime` move
/// can never collide with a same-instant write. (`SystemTime + Duration` is
/// not a const operation, so these are fns.)
fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn t1() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_010)
}

fn credentials_path() -> std::path::PathBuf {
    crate::claude::claude_credentials_path().expect("credentials path")
}

fn seed_credentials_file(at: SystemTime) {
    let path = credentials_path();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("claude dir");
    std::fs::write(&path, b"{}").expect("credentials file");
    set_mtime(&path, at);
}

/// Persist `active` as the configured active profile and give it a usage
/// cache stamped at `at`. The digest reads the raw state value, so `active`
/// need not name a stored profile unless the test drives `switch`.
fn seed_state(active: &str, at: SystemTime) {
    save_app_state(&AppState {
        active_profile: Some(ProfileName::from(active)),
        profiles: vec![ProfileName::from(active)],
        ..Default::default()
    })
    .expect("save state");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(active),
        USAGE_CACHE_FILE,
        &UsageInfo::default(),
    );
    let cache = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from(active),
        USAGE_CACHE_FILE,
    )
    .expect("usage cache path");
    set_mtime(&cache, at);
}

fn third_party_cache_path(name: &str) -> std::path::PathBuf {
    crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from(name),
        THIRD_PARTY_CACHE_FILE,
    )
    .expect("third-party cache path")
}

/// A saved api-key profile as the active one, with the provider cache its own
/// fetch leg writes stamped at `at` — bytes in the shape that leg writes, driven
/// through the production reader like every other consumer. No
/// `usage_cache.json`: that leg never writes one, which is the whole point of
/// the selection under test.
///
/// `base_url` decides which HALF of the api-key set this is: a recognised
/// provider host, or a generic endpoint clauth has no typed integration for.
/// Both are fetched and cached the same way, and only the latter can catch a
/// selector keyed on `is_third_party`.
fn seed_api_key_state(name: &str, base_url: &str, at: SystemTime) {
    crate::profile::save_profile(&crate::profile::Profile::new(
        name.to_string(),
        Some(base_url.to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save api-key profile");
    save_app_state(&AppState {
        active_profile: Some(ProfileName::from(name)),
        profiles: vec![ProfileName::from(name)],
        ..Default::default()
    })
    .expect("save state");
    let path = third_party_cache_path(name);
    std::fs::write(&path, crate::testutil::THIRD_PARTY_CACHE_BYTES).expect("provider cache");
    set_mtime(&path, at);
}

fn drive<F>(fut: F) -> CallToolResult
where
    F: std::future::Future<Output = Result<CallToolResult, ErrorData>>,
{
    // `monitor`'s wait loops sleep on tokio timers, which a bare current-thread
    // runtime does not arm.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(fut)
        .expect("tool returns a tool result, never a transport error")
}

fn block_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("payload text")
}

/// The digest-bearing session read: `profiles({scope: "session"})`, the
/// folded-in former `which` tool. Returns the reply's prose.
fn call_session(server: &ClauthServer) -> String {
    block_text(&drive(server.profiles(Parameters(ProfilesArgs {
        names: None,
        scope: Some("session".to_string()),
    }))))
}

/// The all-scope roster, which deliberately carries no digest.
fn call_roster(server: &ClauthServer) -> String {
    block_text(&drive(server.profiles(Parameters(ProfilesArgs {
        names: None,
        scope: None,
    }))))
}

fn call_switch(server: &ClauthServer, name: &str) -> String {
    block_text(&drive(server.switch_profile(Parameters(SwitchArgs {
        name: name.to_string(),
    }))))
}

/// `monitor` on named jobs (its job mode).
fn call_monitor_ids(server: &ClauthServer, job_ids: &[&str], wait_secs: u64) -> String {
    block_text(&drive(server.monitor_with(
        MonitorArgs {
            job_ids: Some(job_ids.iter().map(|s| (*s).to_string()).collect()),
            wait_secs: Some(wait_secs),
            return_on: None,
            cancel: None,
        },
        ProgressSink::none(),
    )))
}

/// `monitor` with no `job_ids` — the state-waiting mode absorbed from the old
/// `watch` tool.
fn call_monitor_state(server: &ClauthServer, wait_secs: u64) -> String {
    block_text(&drive(server.monitor_with(
        MonitorArgs {
            job_ids: None,
            wait_secs: Some(wait_secs),
            return_on: None,
            cancel: None,
        },
        ProgressSink::none(),
    )))
}

/// A background `delegate`, the one reply shape whose digest each handle site
/// folds for itself. Returns the reply's prose.
///
/// `cwd` points at nothing on purpose: each detached task then stops at the cwd
/// gate before any `claude` spawn, and `HomeSandbox::drop` blocks on its
/// completion signal. Depth is pinned to 0 because a suite running INSIDE a
/// delegate would otherwise refuse at the recursion guard, ahead of the fold
/// under test.
///
/// # Safety
/// `set_var`/`remove_var` are unsafe in Rust 2024 (not thread-safe). The
/// caller's `HomeSandbox` holds `HOME_TEST_LOCK`, which is the serialization,
/// and the prior value is restored before this returns.
fn call_delegate_background(
    server: &ClauthServer,
    home: &HomeSandbox,
    profiles: &[&str],
) -> String {
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the caller's `HomeSandbox` lock.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, "0") };
    let result = drive(
        server.delegate_with(
            DelegateArgs {
                profiles: Some(profiles.iter().map(|p| (*p).to_string()).collect()),
                prompt: Some("hi".to_string()),
                prompt_file: None,
                model: None,
                cwd: Some(
                    home.home()
                        .join("does-not-exist")
                        .to_string_lossy()
                        .into_owned(),
                ),
                env: None,
                args: None,
                timeout_secs: None,
                idle_secs: None,
                resume: None,
                isolated: None,
                background: Some(true),
            },
            ProgressSink::none(),
        ),
    );
    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
    block_text(&result)
}

/// The full fresh-server fixture every test starts from: one active profile,
/// a usage cache, and a credentials file, all stamped at `t0()`.
fn seeded_world() {
    seed_state("work", t0());
    seed_credentials_file(t0());
}

/// [`seeded_world`] plus a second delegable account, so the fan-out arm has two
/// real targets. `work` stays active, so the digest's own observables are the
/// ones `seeded_world` stamped.
fn seeded_world_with_a_second_account() {
    seeded_world();
    save_app_state(&AppState {
        active_profile: Some(ProfileName::from("work")),
        profiles: vec![ProfileName::from("work"), ProfileName::from("spare")],
        ..Default::default()
    })
    .expect("save state");
}

#[test]
fn a_first_digest_call_reports_nothing() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let text = call_session(&server);
    assert!(
        !text.contains("since your last call"),
        "the first digest call establishes the baseline and must not claim a \
         comparison it never made: {text}",
    );
}

/// THE sharing trap: rmcp clones the handler per request, so a baseline stored
/// as a plain field gives every clone its own and the feature reports nothing
/// forever. A clone must compare against the ORIGINAL's baseline.
#[test]
fn a_server_clone_shares_the_digest_baseline() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    seed_state("other", t0());
    let clone = server.clone();
    let text = call_session(&clone);
    assert!(
        text.contains("since your last call: active profile `work` → `other`"),
        "a clone must see the original's baseline and report what moved: {text}",
    );
}

#[test]
fn reporting_consumes_the_delta() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    set_mtime(&credentials_path(), t1());
    let second = call_session(&server);
    assert!(
        second.contains("since your last call: credentials file rewritten"),
        "the moved observable is reported: {second}",
    );

    let third = call_session(&server);
    assert!(
        !third.contains("since your last call"),
        "a reported change is consumed: the third call must not re-report it: {third}",
    );
}

/// Linux reports mtimes in nanoseconds. Truncated to milliseconds, two writes
/// landing inside one millisecond read as one and the second is lost — the
/// mtime-as-change-detector trap this project has paid for before.
#[test]
fn a_sub_millisecond_mtime_move_is_still_a_change() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    let bumped = t0() + Duration::from_micros(500);
    set_mtime(&credentials_path(), bumped);
    // Fixture control: a filesystem that rounds the stamp away leaves no
    // sub-millisecond move to catch, and the assertion below would then pass
    // for the wrong reason.
    assert_eq!(
        std::fs::metadata(credentials_path())
            .expect("credentials metadata")
            .modified()
            .expect("credentials mtime"),
        bumped,
        "the sandbox filesystem must keep the sub-millisecond stamp",
    );

    let text = call_session(&server);
    assert!(
        text.contains("since your last call: credentials file rewritten"),
        "a write 500µs after the baseline is a write: {text}",
    );
}

/// The all-scope roster neither carries nor consumes the digest: it is already
/// a fresh read of the same state, so a delta beside it buys nothing — and
/// swallowing the delta there would mute it for every later reporter. (The
/// session-scope arm DOES carry one; it inherited the folded-in `which`'s
/// role.)
#[test]
fn the_all_scope_roster_carries_no_digest_and_consumes_nothing() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    set_mtime(&credentials_path(), t1());
    let roster = call_roster(&server);
    assert!(
        !roster.contains("since your last call"),
        "the all-scope roster has no live footer and no digest: {roster}",
    );

    let text = call_session(&server);
    assert!(
        text.contains("since your last call: credentials file rewritten"),
        "a roster call between the change and the report must not swallow it",
    );
}

/// Seed two cleanly-linked registered profiles, active + target, the shape a
/// successful switch needs (mirrors the switch-tool suite's fixture). Gated
/// with its only caller below: ungated it is dead code on the Windows leg,
/// which lints at `-D warnings`.
#[cfg(unix)]
fn seed_switchable_pair() {
    use crate::claude::force_link_profile_credentials;
    use crate::profile::{ClaudeCredentials, OAuthToken, Profile, save_profile};

    for name in ["active", "target"] {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("at-{name}"),
                refresh_token: Some(format!("rt-{name}")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        save_profile(&p).expect("save profile");
    }
    force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    save_app_state(&AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into(), "target".into()],
        ..Default::default()
    })
    .expect("save state");
}

/// A switch that ran never reports its own write: its reply's
/// `previous`/`active` IS the report, and the reseed means the next call does
/// not echo the switch back as news from elsewhere.
// Unix-gated with the switch-tool suite it mirrors: the mutation path it
// drives is the one that suite keeps off the Windows leg.
#[test]
#[cfg(unix)]
fn a_successful_switch_reseeds_rather_than_reporting_its_own_write() {
    let _home = HomeSandbox::new();
    seed_switchable_pair();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    let switched = call_switch(&server, "target");
    assert!(
        switched.contains("switched the global active profile from `active` to `target`"),
        "fixture control: the switch ran",
    );
    assert!(
        !switched.contains("since your last call"),
        "a switch reply must not report its own write as external news: {switched}",
    );

    let after = call_session(&server);
    assert!(
        !after.contains("since your last call"),
        "the reseed consumed the switch's write; the next reply must stay silent: {after}",
    );
}

/// A switch that refused BEFORE any mutation ran wrote nothing, so any delta
/// it sees is external news and reports exactly like the session read does.
#[test]
fn a_refused_switch_reports_external_changes() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    seed_state("other", t0());
    let refused = call_switch(&server, "ghost");
    assert!(
        refused.contains("switch failed"),
        "fixture control: it refused",
    );
    assert!(
        refused.contains("since your last call: active profile `work` → `other`"),
        "a pre-mutation refusal carries the digest like the session read does: {refused}",
    );
}

#[test]
fn state_wait_first_call_arms_the_baseline() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let armed = call_monitor_state(&server, 0);
    assert!(
        armed.contains("monitor armed"),
        "arming is not a comparison: {armed}",
    );
}

/// The long-poll half of the contract: a change landing mid-wait wakes the
/// call at the next poll slice, not at the deadline.
#[test]
fn state_wait_returns_as_soon_as_something_moves() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    let path = credentials_path();
    let mover = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        set_mtime(&path, t1());
    });
    let start = Instant::now();
    let text = call_monitor_state(&server, 60);
    let elapsed = start.elapsed();
    assert!(
        text.contains("monitor: since your last call: credentials file rewritten"),
        "the mid-wait change must be caught: {text}",
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "a change 300ms in must return at the next poll slice, not the 60s \
         deadline (took {elapsed:?})",
    );
    mover.join().expect("mover thread");
}

/// The usage-cache observable is KEYED on the profile it was read from: two
/// profiles' caches are different files, so a profile change is no
/// `usage_cache` event. Reporting the incomparable pair as a refresh would be
/// a statement to the model that nothing made true.
#[test]
fn a_profile_change_is_never_reported_as_a_usage_cache_refresh() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    // Another profile, carrying its own cache at its own stamp.
    seed_state("other", t1());
    let reported = call_monitor_state(&server, 0);
    assert!(
        reported.contains("since your last call: active profile `work` → `other`"),
        "the profile change is the news: {reported}",
    );
    assert!(
        !reported.contains("usage cache refreshed"),
        "two profiles' caches are not comparable, so no refresh may be claimed: {reported}",
    );

    // Consuming the profile change re-keys the cache baseline, so the false
    // refresh cannot land one call later either.
    let next = call_session(&server);
    assert!(
        !next.contains("since your last call"),
        "the re-key onto the new profile's cache is silent: {next}",
    );
}

/// Finding 8, the replication defect: the third-party fetch leg never writes
/// `usage_cache.json`, so keying a third-party active profile's refresh
/// observable on that file means the event can never fire — an account the
/// scheduler refreshes hourly reads as frozen forever. `daemon/status_json.rs`
/// was already fixed for this exact defect; the digest re-introduced it.
#[test]
fn a_third_party_profiles_refresh_fires_off_the_cache_its_own_leg_writes() {
    let _home = HomeSandbox::new();
    seed_api_key_state("vendor", "https://api.deepseek.com/anthropic", t0());
    seed_credentials_file(t0());
    let server = ClauthServer::new();
    let _ = call_session(&server);

    set_mtime(&third_party_cache_path("vendor"), t1());
    let text = call_session(&server);
    assert!(
        text.contains("since your last call: usage cache refreshed"),
        "the file this account's own leg refreshes is the one the event watches: {text}",
    );
}

/// The half a selector keyed on `is_third_party` still gets wrong: a GENERIC
/// api-key endpoint (no typed integration, so `provider` is `None`) is fetched
/// and cached by the same leg — `third_party_entry_for` builds a
/// `ThirdPartyTarget::Generic` for it — so its refresh lives in the same file.
/// Keying on "is this a recognised provider" answers a different question and
/// leaves every local llama, aggregator and self-hosted endpoint watching a file
/// nothing writes.
#[test]
fn a_generic_api_key_profiles_refresh_fires_off_the_cache_its_own_leg_writes() {
    let _home = HomeSandbox::new();
    seed_api_key_state("litellm", "http://127.0.0.1:4000", t0());
    seed_credentials_file(t0());
    let server = ClauthServer::new();
    let _ = call_session(&server);

    set_mtime(&third_party_cache_path("litellm"), t1());
    let text = call_session(&server);
    assert!(
        text.contains("since your last call: usage cache refreshed"),
        "a generic api-key account's own cache is the one the event watches: {text}",
    );
}

/// The other half of the same selection, and the one a "watch both files"
/// shortcut would fail: a third-party profile can carry a LEFTOVER
/// `usage_cache.json` from an earlier OAuth life (one operator profile holds a
/// 42-byte one beside a current provider cache). Nothing refreshes it, so a
/// touch of it is not news about this account.
#[test]
fn a_third_party_profiles_leftover_oauth_cache_is_not_the_file_watched() {
    let _home = HomeSandbox::new();
    seed_api_key_state("vendor", "https://api.deepseek.com/anthropic", t0());
    seed_credentials_file(t0());
    let stale_oauth_cache = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("vendor"),
        USAGE_CACHE_FILE,
    )
    .expect("cache path");
    std::fs::write(&stale_oauth_cache, b"{}").expect("leftover oauth cache");
    set_mtime(&stale_oauth_cache, t0());
    let server = ClauthServer::new();
    let _ = call_session(&server);

    set_mtime(&stale_oauth_cache, t1());
    let text = call_session(&server);
    assert!(
        !text.contains("usage cache refreshed"),
        "a file nothing refreshes carries no news about this account: {text}",
    );

    // Positive control on the same fixture, so the silence above is the
    // selection and not a digest that reports nothing at all.
    set_mtime(&third_party_cache_path("vendor"), t1());
    let moved = call_session(&server);
    assert!(
        moved.contains("since your last call: usage cache refreshed"),
        "the account's own cache still fires: {moved}",
    );
}

#[test]
fn state_wait_timeout_reports_unchanged_with_waited_secs() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();
    let _ = call_session(&server);

    let start = Instant::now();
    let text = call_monitor_state(&server, 1);
    assert!(
        text.contains("monitor: no change after"),
        "timeout says so: {text}"
    );
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "the wait must actually elapse before the unchanged answer: {text}",
    );
}

#[test]
fn state_wait_answers_prose() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let changed = call_monitor_state(&server, 0);
    assert!(
        serde_json::from_str::<serde_json::Value>(&changed).is_err(),
        "the state-wait reply must be prose, not a JSON blob: {changed}",
    );
}

/// No lock may span a sleep in the digest machinery: the state wait runs up
/// to 3600s, and a baseline lock held across its slices would stall every other
/// digest-bearing reply on the server. The shape is what's checkable — a
/// timing probe stays green under a slice-wise violation, because the mutex
/// futex hands the lock to a parked waiter inside one 200ms slice — so this is
/// a source guard (the out.rs pattern): the sleeping function must not lock,
/// and the locking functions must not sleep.
///
/// Ceiling: it reads `src/mcp/digest.rs` textually, so it catches a lock or a
/// sleep landing in the named function bodies, not one laundered through a
/// fresh helper those bodies call. That requires deliberately adding a helper,
/// which is the point where a review reads the lock order anyway.
#[test]
fn the_sleeping_function_never_locks_and_the_locking_functions_never_sleep() {
    let src = include_str!("../../src/mcp/digest.rs");
    fn body(src: &str, name: &str) -> String {
        let start = src
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("{name} not found"));
        // To the next method at the same indent, private or `pub(super)`,
        // sync or async — an async spelling left off this list silently folds
        // the NEXT method's body into this one's and inverts the assertions.
        let rest = &src[start..];
        let end = [
            "\n    fn ",
            "\n    pub(super) fn ",
            "\n    async fn ",
            "\n    pub(super) async fn ",
        ]
        .iter()
        .filter_map(|pat| rest.find(pat))
        .min()
        .unwrap_or(rest.len());
        // Comment lines out: the scanned contract is about CODE, and the doc
        // comments around these methods name `sleep` and `lock` in prose.
        rest[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let watch = body(src, "watch");
    assert!(
        !watch.contains(".lock()") && !watch.contains("lock("),
        "watch sleeps, so it must not hold the baseline lock anywhere in its \
         body: {watch}",
    );
    // The new spelling, since slice 2 turned the wait loops async so they can
    // await a progress notification: a blocking sleep here would park the
    // reactor thread for the whole wait, and no guard may straddle the await.
    // The slice itself is the sink's shared `sleep_or_cancelled`, which is also
    // what makes an abandoned call end here instead of running out its hour, so
    // a raw sleep back in this body would silently drop cancellation too.
    assert!(
        watch.contains("sleep_or_cancelled") && !watch.contains("std::thread::sleep"),
        "watch sleeps through the shared cancel-aware slice, never by blocking \
         its thread: {watch}",
    );
    for locker in ["report", "reseed"] {
        let body = body(src, locker);
        assert!(
            !body.contains("sleep"),
            "{locker} takes the baseline lock, so it must not sleep inside it: {body}",
        );
    }
}

/// The done envelope of a collected job is a digest-bearing reply too (it
/// folds `live_usage`), so it reports and consumes like the session read does.
#[test]
fn monitor_done_envelope_reports_the_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let envelope = serde_json::json!({
        "profile": "work",
        "is_error": false,
        "result": "all done",
    });
    jobs::write_done("d-digest-0", "work", 1, None, None, false, envelope.clone())
        .expect("write job");
    let first = call_monitor_ids(&server, &["d-digest-0"], 0);
    assert!(
        !first.contains("since your last call"),
        "first digest call seeds, even through a collected job: {first}",
    );

    jobs::write_done("d-digest-1", "work", 1, None, None, false, envelope).expect("write job");
    set_mtime(&credentials_path(), t1());
    let second = call_monitor_ids(&server, &["d-digest-1"], 0);
    assert!(
        second.contains("since your last call: credentials file rewritten"),
        "the done envelope reports what moved since the first call: {second}",
    );
}

/// Seed one finished job whose envelope reads `all done`.
fn seed_done_job(id: &str) {
    jobs::write_done(
        id,
        "work",
        1,
        None,
        None,
        false,
        serde_json::json!({ "profile": "work", "is_error": false, "result": "all done" }),
    )
    .expect("write job");
}

/// A batch is ONE call, so its digest rides the reply once, top-level beside
/// the job lines, and is consumed by that one report.
#[test]
fn monitor_batch_carries_one_top_level_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    seed_done_job("d-batch-0");
    seed_done_job("d-batch-1");
    let first = call_monitor_ids(&server, &["d-batch-0", "d-batch-1"], 0);
    assert_eq!(
        first.matches("since your last call").count(),
        0,
        "the first digest call seeds, through a batch like anywhere else: {first}",
    );

    set_mtime(&credentials_path(), t1());
    seed_done_job("d-batch-2");
    seed_done_job("d-batch-3");
    let second = call_monitor_ids(&server, &["d-batch-2", "d-batch-3"], 0);
    assert_eq!(
        second.matches("since your last call").count(),
        1,
        "the digest rides the batch reply exactly once, on its own last line: {second}",
    );
    assert!(
        second.contains("since your last call: credentials file rewritten"),
        "and it names what moved: {second}",
    );

    let after = call_session(&server);
    assert!(
        !after.contains("since your last call"),
        "the batch reported the change, so the batch consumed it: {after}",
    );
}

/// The digest a batch consumes must also be RENDERED, or it is lost for good.
#[test]
fn monitor_batch_prose_renders_the_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    seed_done_job("d-bprose-0");
    let _ = call_monitor_ids(&server, &["d-bprose-0"], 0);

    set_mtime(&credentials_path(), t1());
    seed_done_job("d-bprose-1");
    // One id renders through the single-job spelling, which carries live
    // usage inline (the several-ids lines stay short instead).
    assert_eq!(
        call_monitor_ids(&server, &["d-bprose-1"], 0),
        "delegate to `work` finished: all done; target `work`: 5h unknown, 7d unknown; \
         since your last call: credentials file rewritten",
    );
}

/// The single background handle reports the digest like every other
/// digest-bearing reply, and REPORTING CONSUMES IT — the field being rendered
/// is only half the contract, and the half a `Reseed` or a second `Report`
/// would break invisibly.
#[test]
fn a_background_handle_reports_the_digest_once_and_consumes_it() {
    let home = HomeSandbox::new();
    seeded_world();
    let server = ClauthServer::new();

    let first = call_delegate_background(&server, &home, &["work"]);
    assert_eq!(
        first.matches("since your last call").count(),
        0,
        "the first digest call seeds, through a handle like anywhere else: {first}",
    );

    set_mtime(&credentials_path(), t1());
    let second = call_delegate_background(&server, &home, &["work"]);
    assert_eq!(
        second.matches("since your last call").count(),
        1,
        "the handle reports what moved, exactly once: {second}",
    );
    assert!(
        second.contains("since your last call: credentials file rewritten"),
        "and it names what moved: {second}",
    );

    let after = call_session(&server);
    assert!(
        !after.contains("since your last call"),
        "the handle reported the change, so the handle consumed it: {after}",
    );
}

/// A fan-out is ONE call, so its digest rides the reply once, at the top level
/// after every target's headroom — never folded per job row.
///
/// This repo has already shipped the per-row spelling once: nesting the digest
/// inside each result put it on the first row only, left the renderer with
/// nothing to print, consumed the delta and dropped it for good. A row-level
/// fold is
/// silent in the prose, so nothing but a count can catch it.
#[test]
fn a_fanout_reply_carries_one_top_level_digest_and_no_row_carries_one() {
    let home = HomeSandbox::new();
    seeded_world_with_a_second_account();
    let server = ClauthServer::new();

    let first = call_delegate_background(&server, &home, &["work", "spare"]);
    assert_eq!(
        first.matches("since your last call").count(),
        0,
        "the first call seeds through a fan-out too: {first}",
    );

    set_mtime(&credentials_path(), t1());
    let second = call_delegate_background(&server, &home, &["work", "spare"]);
    assert_eq!(
        second.matches("since your last call").count(),
        1,
        "one call is one digest: two rows must not each fold a copy, and a row \
         that folded one would eat the delta the reply then cannot print: {second}",
    );
    assert!(
        second.ends_with("since your last call: credentials file rewritten"),
        "top level, after both targets' headroom — not spliced between two job \
         rows: {second}",
    );

    let after = call_session(&server);
    assert!(
        !after.contains("since your last call"),
        "the fan-out reported the change, so the fan-out consumed it: {after}",
    );
}

/// F7: a blocking `delegate` reply that will never be sent must not spend the
/// digest on its way to being dropped.
///
/// A caller can abandon the request while `ProfileRuntime::acquire` is blocked;
/// the run is stopped, its envelope comes back through the join, and the handler
/// folds a reply that rmcp then drops for a cancelled request. Reporting the
/// delta there consumes it, so the news is gone by the time a reply someone
/// actually reads is built.
///
/// `Skip` rather than `Reseed`: nothing of clauth's moved here, so the baseline
/// must stay exactly where it was and the delta must survive to the next reply.
#[test]
fn an_abandoned_blocking_delegate_reply_never_consumes_the_digest() {
    let _home = HomeSandbox::new();
    seeded_world();
    let tracker = DigestTracker::new();
    let fold = |abandoned: bool| {
        fold_delegate_live_usage(
            serde_json::json!({"profile": "work", "result": "done"}),
            &crate::profile::ProfileName::from("work"),
            delegate_call_endpoint("work", &std::collections::HashMap::new()),
            None,
            0,
            super::delegate_digest_mode(&tracker, abandoned),
        )
    };

    // A first call only arms the baseline.
    assert!(
        fold(false).get("since_your_last_call").is_none(),
        "fixture control: the first call establishes the baseline",
    );
    set_mtime(&credentials_path(), t1());

    let dropped = fold(true);
    assert!(
        dropped.get("since_your_last_call").is_none(),
        "an abandoned reply reports nothing — there is no reader: {dropped}",
    );

    let real = fold(false);
    assert!(
        real.get("since_your_last_call").is_some(),
        "and crucially it consumed nothing, so the next reply that IS delivered \
         still carries the change: {real}",
    );
}
