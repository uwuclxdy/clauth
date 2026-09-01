use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::oauth::RefreshError;

use crate::profile::DEFAULT_REFRESH_INTERVAL_MS as REFRESH_INTERVAL_MS;

use super::{
    ActivityStore, ClaudeRollingPacing, EpochMs, LastFetchedAt, ProfileActivity,
    RESET_ANCHOR_GRACE_MS, SuppressedGenericStore, ThirdPartyEntry, TokenEntry,
    anchor_post_reset_oauth, clear_activity, clear_orphaned_forced, collect_oauth_seed_names,
    collect_third_party_entries, collect_tokens, filter_suppressed, mark_activity,
    memoized_identity, partition_due, should_anchor_fetch, window_lapsed,
};

fn token(name: &str) -> TokenEntry {
    TokenEntry {
        name: crate::profile::ProfileName::from(name),
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
        auto_start: false,
        access_expires_at: None,
        auth_broken: false,
        may_open_window: true,
    }
}

/// An OAuth-credentialed profile, optionally disabled, for the
/// `collect_tokens`/`collect_third_party_entries` work-list exclusion tests.
fn oauth_profile_disabled(name: &str, disabled: bool) -> crate::profile::Profile {
    use crate::profile::{ClaudeCredentials, OAuthToken};

    let mut p = crate::profile::Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    p.disabled = disabled;
    p
}

/// An enabled OAuth profile that opts into both auto-start and its queue.
fn auto_start_queue_profile(name: &str) -> crate::profile::Profile {
    let mut profile = oauth_profile_disabled(name, false);
    profile.auto_start = true;
    profile
}

// A disabled account must not enter the scheduler's per-profile work list at
// all: no polling, no rotation, no auto-start ping, no stuck-429 distrust —
// all downstream of never appearing in the OAuth `TokenEntry` snapshot.
#[test]
fn collect_tokens_excludes_disabled_profiles_includes_enabled_siblings() {
    use crate::profile::{AppConfig, AppState};

    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            oauth_profile_disabled("off", true),
            oauth_profile_disabled("on", false),
        ],
    };

    let entries = collect_tokens(&config);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains(&"off"),
        "a disabled account must never enter the poll/rotate work list"
    );
    assert!(
        names.contains(&"on"),
        "an enabled sibling must still be collected for polling"
    );
}

// The DISPLAY seed is the complement of `collect_tokens`: it INCLUDES a disabled
// OAuth profile (so its cached tier/windows render) — the exact hole behind the
// stale-tier bug — while the work-list above still excludes it. End-to-end: a
// disabled profile's on-disk usage cache lands in the live store via
// `bootstrap_fetch(collect_oauth_seed_names(..))`, and seeding it never widens
// the poll list. A credential-less profile has no oauth cache, so it is not seeded.
#[test]
fn collect_oauth_seed_names_includes_disabled_and_bootstrap_seeds_its_cache() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::{UsageInfo, UsageWindow};
    let _home = crate::testutil::HomeSandbox::new();

    let mut credless = crate::profile::Profile::new("credless".to_string(), None, None);
    credless.disabled = true;
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            oauth_profile_disabled("off", true),
            oauth_profile_disabled("on", false),
            credless,
        ],
    };

    let seed = collect_oauth_seed_names(&config);
    assert!(
        seed.contains(&"off".to_string()),
        "the display seed must include a disabled OAuth profile: {seed:?}"
    );
    assert!(
        seed.contains(&"on".to_string()),
        "and its enabled sibling: {seed:?}"
    );
    assert!(
        !seed.contains(&"credless".to_string()),
        "a credential-less profile has no oauth cache to seed: {seed:?}"
    );

    // End-to-end: the disabled profile's on-disk cache lands in the live store.
    let info = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    crate::testutil::register_names(&["off"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("off"),
        USAGE_CACHE_FILE,
        &info,
    );

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: super::StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    super::bootstrap_fetch(&store, &status, &last_fetched, &seed, REFRESH_INTERVAL_MS);

    let seeded = store.lock().unwrap().get("off").cloned();
    assert_eq!(
        seeded.and_then(|i| i.five_hour.map(|w| w.utilization)),
        Some(42.0),
        "a disabled profile's cached window is seeded for display"
    );

    // Invariant preserved: seeding the store never widens the poll work-list.
    let poll_names: Vec<String> = collect_tokens(&config)
        .iter()
        .map(|e| e.name.to_string())
        .collect();
    assert!(
        !poll_names.contains(&"off".to_string()),
        "seeding a disabled profile must not make it pollable: {poll_names:?}"
    );
}

// Third-party (api-key) leg's own work list must honor the same exclusion.
#[test]
fn collect_third_party_entries_excludes_disabled_profiles_includes_enabled_siblings() {
    let mut off = crate::profile::Profile::new(
        "off".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    off.disabled = true;
    let on = crate::profile::Profile::new(
        "on".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );

    let entries = collect_third_party_entries(&[off, on]);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains(&"off"),
        "a disabled third-party account must never enter the poll work list"
    );
    assert!(
        names.contains(&"on"),
        "an enabled third-party sibling must still be collected"
    );
}

/// The same credential test, tightened: an EMPTY or whitespace-only api key is
/// no credential, so the profile never enters the poll work list. A keyed
/// sibling still does.
#[test]
fn collect_third_party_entries_skips_an_empty_key_profile() {
    let keyed = crate::profile::Profile::new(
        "keyed".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let empty = crate::profile::Profile::new(
        "empty".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some(String::new()),
    );
    let space = crate::profile::Profile::new(
        "space".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("  \t ".to_string()),
    );

    let collected = collect_third_party_entries(&[keyed, empty, space]);
    let names: Vec<&str> = collected.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"keyed"),
        "a keyed third-party account is still collected"
    );
    assert!(
        !names.contains(&"empty"),
        "an empty-key account must never enter the poll work list: {names:?}"
    );
    assert!(
        !names.contains(&"space"),
        "a whitespace-key account must never enter the poll work list: {names:?}"
    );
}

/// Every profile uses the same fixed `REFRESH_INTERVAL_MS` cadence: a
/// never-fetched profile is due once `now` reaches the interval, a just-fetched
/// one is not due until exactly one interval has elapsed, and the published
/// next-time is always `last_fetched + REFRESH_INTERVAL_MS`.
#[test]
fn partition_due_uses_fixed_interval() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a")];
    let base = 1_700_000_000_000u64; // realistic epoch-ms

    // Never fetched: last = 0, next = REFRESH_INTERVAL_MS, due at any real `now`.
    let (due, next) = partition_due(
        &snapshot,
        base,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert_eq!(due.len(), 1, "a never-fetched profile is due");
    assert_eq!(next.get("a").copied(), Some(REFRESH_INTERVAL_MS));

    // Just fetched: not due one ms later.
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));
    let (due, next) = partition_due(
        &snapshot,
        base + 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert!(due.is_empty(), "not due one ms after a fetch");
    assert_eq!(next.get("a").copied(), Some(base + REFRESH_INTERVAL_MS));

    // Exactly one interval later: due again.
    let (due, _) = partition_due(
        &snapshot,
        base + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert_eq!(due.len(), 1, "due once the fixed interval has elapsed");
}

// Post-reset fire-once anchoring. `now` is a realistic epoch-ms.
const ANCHOR_NOW: u64 = 1_700_000_000_000;

/// `should_anchor_fetch`: fires exactly once when a reset has crossed since our
/// last fetch and the grace has elapsed, and stays quiet otherwise.
#[test]
fn should_anchor_fetch_fires_once_after_reset_plus_grace() {
    let now = ANCHOR_NOW;
    let reset = now - RESET_ANCHOR_GRACE_MS; // reset landed exactly GRACE ago
    let last = reset - 60_000; // fetched a minute before the reset

    assert!(
        should_anchor_fetch(Some(reset), last, now, RESET_ANCHOR_GRACE_MS),
        "reset crossed since last fetch, grace elapsed → fire",
    );
    assert!(
        should_anchor_fetch(
            Some(now - RESET_ANCHOR_GRACE_MS),
            last,
            now,
            RESET_ANCHOR_GRACE_MS
        ),
        "the now == reset + grace boundary still fires",
    );
    assert!(
        !should_anchor_fetch(
            Some(now - RESET_ANCHOR_GRACE_MS + 1),
            last,
            now,
            RESET_ANCHOR_GRACE_MS
        ),
        "grace not yet elapsed → hold",
    );
    assert!(
        !should_anchor_fetch(Some(reset), reset, now, RESET_ANCHOR_GRACE_MS),
        "already fetched at/after the reset → self-limited, no re-fire",
    );
    assert!(
        !should_anchor_fetch(None, last, now, RESET_ANCHOR_GRACE_MS),
        "no reset stamp → never",
    );
}

/// `anchor_post_reset_oauth` schedules only the eligible profile: it lands in
/// `due` with its countdown stamped to `now`, while an excluded one, one already
/// fetched post-reset, and one with no reset stamp are all left alone, and a
/// profile already in `due` is not duplicated. This reds if the due-push, the
/// exclusion, or the dedup is dropped.
#[test]
fn anchor_post_reset_oauth_schedules_only_eligible_profiles() {
    let now = ANCHOR_NOW;
    let reset = now - RESET_ANCHOR_GRACE_MS - 1_000; // reset + grace comfortably passed
    let last_before = EpochMs::from_millis(reset - 60_000); // fetched before the reset
    let last_after = EpochMs::from_millis(reset + 1_000); // already fetched post-reset

    let snapshot = vec![
        token("due"),
        token("excluded"),
        token("already"),
        token("fetched"),
        token("noreset"),
    ];
    let resets = HashMap::from([
        ("due".to_string(), reset),
        ("excluded".to_string(), reset),
        ("already".to_string(), reset),
        ("fetched".to_string(), reset),
        // "noreset" carries no stamp.
    ]);
    let last_fetched = HashMap::from([
        ("due".to_string(), last_before),
        ("excluded".to_string(), last_before),
        ("already".to_string(), last_before),
        ("fetched".to_string(), last_after),
        ("noreset".to_string(), last_before),
    ]);
    let excluded = HashSet::from(["excluded".to_string()]);
    let mut due = vec![token("already")]; // already scheduled by partition_due
    let mut next: HashMap<String, u64> = HashMap::new();

    anchor_post_reset_oauth(
        &snapshot,
        &resets,
        &last_fetched,
        &excluded,
        &mut due,
        &mut next,
        now,
    );

    let due_names: HashSet<&str> = due.iter().map(|e| e.name.as_str()).collect();
    assert!(
        due_names.contains("due"),
        "eligible post-reset profile is scheduled"
    );
    assert_eq!(
        next.get("due").copied(),
        Some(now),
        "its countdown is stamped to now"
    );
    assert!(
        !due_names.contains("excluded"),
        "Refreshing/Switching profile is not scheduled"
    );
    assert!(
        !due_names.contains("fetched"),
        "a profile already fetched post-reset is not re-scheduled",
    );
    assert!(
        !due_names.contains("noreset"),
        "a profile with no reset stamp is not scheduled"
    );
    assert_eq!(
        due.iter().filter(|e| e.name == "already").count(),
        1,
        "an already-due profile is not duplicated",
    );
    assert!(
        !next.contains_key("excluded")
            && !next.contains_key("fetched")
            && !next.contains_key("noreset"),
        "skipped profiles get no countdown stamp",
    );
}

/// `spent_skip_set` (the `refresh_spent_accounts` OFF gate): only an unforced,
/// already-fetched, spent account is skipped. A forced (`r`) account, a never-
/// fetched one, a below-cap one, and one whose spent window has lapsed are all
/// absent from the set — the last two are how a reset gets observed.
#[test]
fn spent_skip_set_selects_only_unforced_spent_accounts() {
    use super::spent_skip_set;
    use crate::usage::{UsageInfo, UsageWindow};

    let now = 1_779_027_600i64; // 2026-05-17 UTC
    let capped = |resets: &str| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 100.0,
            resets_at: Some(resets.to_string()),
        }),
        ..Default::default()
    };
    let store: HashMap<String, UsageInfo> = HashMap::from([
        ("spent".to_string(), capped("2999-01-01T00:00:00+00:00")),
        (
            "spent_forced".to_string(),
            capped("2999-01-01T00:00:00+00:00"),
        ),
        ("lapsed".to_string(), capped("2020-01-01T00:00:00+00:00")),
        (
            "busy".to_string(),
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: 40.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                ..Default::default()
            },
        ),
        // "fresh" has no store entry → never fetched → always polled.
    ]);
    let forced: HashSet<String> = HashSet::from(["spent_forced".to_string()]);

    let snapshot = vec![
        token("spent"),
        token("spent_forced"),
        token("lapsed"),
        token("busy"),
        token("fresh"),
    ];
    let skip = spent_skip_set(&snapshot, &forced, &store, now);
    assert_eq!(
        skip,
        HashSet::from(["spent".to_string()]),
        "only the unforced spent account is skipped; forced/lapsed/below-cap/never-fetched poll",
    );
}

/// Profiles mid-refresh are excluded from the due set even when their interval
/// has elapsed, but their countdown still publishes so the UI shows when they
/// become eligible again.
#[test]
fn partition_due_excludes_refreshing() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a")];

    mark_activity(
        &activity,
        &crate::profile::ProfileName::from("a"),
        ProfileActivity::Refreshing,
    );

    let (due, next) = partition_due(
        &snapshot,
        REFRESH_INTERVAL_MS + 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert!(due.is_empty(), "refreshing profiles are excluded from due");
    assert!(
        next.contains_key("a"),
        "countdown still publishes for excluded profiles"
    );
}

/// A profile whose switch gate is in flight (`Switching`) is excluded like a
/// `Refreshing` one: a fetch worker would re-mark it `Queued`/`Fetching`,
/// overwriting the pending-switch mark that `switch_gate_in_flight` keys on.
#[test]
fn partition_due_excludes_switching() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a")];

    mark_activity(
        &activity,
        &crate::profile::ProfileName::from("a"),
        ProfileActivity::Switching,
    );

    let (due, next) = partition_due(
        &snapshot,
        REFRESH_INTERVAL_MS + 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert!(due.is_empty(), "mid-switch profiles are excluded from due");
    assert!(
        next.contains_key("a"),
        "countdown still publishes for excluded profiles"
    );
}

/// A quarantined (`auth_broken`) profile's poll spends a guaranteed-dead
/// 401 → refresh → 400 pair against the token endpoint, so partition widens
/// its cadence by `AUTH_BROKEN_BACKOFF_MS` — computed from the live flag,
/// never baked into the `last_fetched` stamp, so any flag lift (login, adopt,
/// carry) snaps the cadence back on the very next tick.
#[test]
fn partition_due_defers_flagged_profiles_until_the_flag_lifts() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let base = 1_700_000_000_000u64;
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));

    let mut flagged = token("a");
    flagged.auth_broken = true;
    let snapshot = vec![flagged];

    // One interval elapsed: an unflagged profile would be due here.
    let at_interval = base + REFRESH_INTERVAL_MS + 1;
    let (due, next) = partition_due(
        &snapshot,
        at_interval,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert!(due.is_empty(), "flagged profile skips the plain cadence");
    assert_eq!(
        next["a"],
        base + REFRESH_INTERVAL_MS + super::AUTH_BROKEN_BACKOFF_MS,
        "published countdown shows the widened deadline"
    );

    // Past the widened deadline it still polls — the poll's own refresh
    // attempt stays a (slow) recovery path.
    let (due, _) = partition_due(
        &snapshot,
        base + REFRESH_INTERVAL_MS + super::AUTH_BROKEN_BACKOFF_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert_eq!(
        due.len(),
        1,
        "a flagged profile still polls after the backoff"
    );

    // Same stamp, flag lifted: due immediately on the plain cadence.
    let unflagged = vec![token("a")];
    let (due, next) = partition_due(
        &unflagged,
        at_interval,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert_eq!(
        due.len(),
        1,
        "an unflagged profile snaps back to the cadence"
    );
    assert_eq!(next["a"], base + REFRESH_INTERVAL_MS);
}

/// The sibling of the `auth_broken` widen above, for the failure it can NEVER
/// cover: a refresh the endpoint rejected without confirming the token is dead
/// (`RefreshError::Transient`) leaves the profile unflagged on purpose, so
/// `auth_broken`'s backoff never applies. Without a ladder of its own, the one
/// failure mode that hits every profile at once — clauth's own request shape
/// drifting — re-hits the token endpoint at the full cadence forever, on every
/// account, with the row saying only `cached`. Same curve and ceiling as the 429
/// ladder, and computed live at partition time so a recovery snaps straight back.
#[test]
fn partition_due_ladders_a_profile_whose_refresh_keeps_failing() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let base = 1_700_000_000_000u64;
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));

    let snapshot = vec![token("a")];
    let streaks = |refresh_fail: u32| {
        HashMap::from([(
            "a".to_string(),
            super::StreakCounts {
                rate_limit: 0,
                refresh_fail,
            },
        )])
    };
    let next_at = |streaks: &HashMap<String, super::StreakCounts>| {
        partition_due(
            &snapshot,
            base,
            &last_fetched,
            &activity,
            REFRESH_INTERVAL_MS,
            streaks,
        )
        .1["a"]
    };

    // Streak 0 is the plain cadence — `rate_limit_backoff_ms(0)` returns a full
    // base step, so an unguarded call would silently defer a healthy profile.
    assert_eq!(
        next_at(&streaks(0)),
        base + REFRESH_INTERVAL_MS,
        "a profile with no refresh failures must not be deferred at all"
    );

    // The ladder climbs: 10s, 30s, 90s… on top of the fixed cadence.
    assert_eq!(next_at(&streaks(1)), base + REFRESH_INTERVAL_MS + 10_000);
    assert_eq!(next_at(&streaks(2)), base + REFRESH_INTERVAL_MS + 30_000);
    assert_eq!(next_at(&streaks(3)), base + REFRESH_INTERVAL_MS + 90_000);

    // …and stops at the same 15-minute ceiling the 429 ladder honors, rather
    // than running away to hours (`rate_limit_backoff_ms` alone is unbounded).
    assert_eq!(
        next_at(&streaks(50)),
        base + REFRESH_INTERVAL_MS + super::MAX_RETRY_AFTER_MS,
        "a deep refresh-fail streak caps at MAX_RETRY_AFTER_MS",
    );

    // A quarantined profile keeps the wider `auth_broken` deferral: that flag
    // means the token is confirmed dead, which outranks "might be a blip".
    let mut flagged = token("a");
    flagged.auth_broken = true;
    let (_, next) = partition_due(
        &[flagged],
        base,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &streaks(1),
    );
    assert_eq!(
        next["a"],
        base + REFRESH_INTERVAL_MS + super::AUTH_BROKEN_BACKOFF_MS,
        "a confirmed-dead token outranks the refresh-fail ladder"
    );
}

/// The two streak axes must move independently, because every other reader
/// means only one of them: `rate_limit` feeds `is_stuck_rate_limited`, the
/// auto-switch freshness bypass and `status.json`'s `stale` — none of which a
/// refresh failure may ever claim. A live body clears both.
#[test]
fn streak_axes_move_independently_and_a_live_body_clears_both() {
    use super::FetchStatus;

    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let update = |status: FetchStatus, refresh_failed: bool| {
        super::update_streaks(
            &streaks,
            &crate::profile::ProfileName::from("a"),
            status,
            refresh_failed,
        )
    };

    // A transient refresh failure bails to `Cached` — it must NOT touch the 429
    // axis, or a client-side bug would report a stuck throttle and let the
    // auto-switch rotate the chain away on it.
    let counts = update(FetchStatus::Cached, true);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (0, 1));
    let counts = update(FetchStatus::Cached, true);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (0, 2));

    // A 429 bumps only its own axis and leaves the refresh count standing.
    let counts = update(FetchStatus::RateLimited, false);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (1, 2));

    // A status that says nothing about either axis holds both — and must not
    // conjure an entry for a profile that has none.
    let counts = update(FetchStatus::Failed, false);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (1, 2));
    assert_eq!(
        super::update_streaks(
            &streaks,
            &crate::profile::ProfileName::from("never-seen"),
            FetchStatus::Failed,
            false
        ),
        super::StreakCounts::default(),
    );
    assert!(
        !streaks.lock().unwrap().contains_key("never-seen"),
        "a no-op update must not insert an empty entry"
    );

    // A live body clears both: whatever went wrong, the profile is serving. This
    // is also the preemptive-rotation case — a refresh can fail while the still
    // valid access token fetches fine, and nothing is degraded yet.
    let counts = update(FetchStatus::Fresh, true);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (0, 0));
    assert!(!streaks.lock().unwrap().contains_key("a"));
}

/// Forced (manual `r`) refetches skip a mid-switch profile for the same
/// reason: scheduling it would overwrite the `Switching` mark and drop the
/// in-flight switch pending state.
#[test]
fn merge_forced_skips_switching() {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    mark_activity(
        &activity,
        &crate::profile::ProfileName::from("switching"),
        ProfileActivity::Switching,
    );

    let snapshot = vec![token("switching"), token("plain")];
    let forced: HashSet<String> = ["switching", "plain"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut due: Vec<TokenEntry> = Vec::new();
    let mut next: HashMap<String, u64> = HashMap::new();

    super::merge_forced(&snapshot, &forced, &mut due, &mut next, &activity, 1);

    assert_eq!(due.len(), 1, "only the unowned profile is scheduled");
    assert_eq!(due[0].name, "plain");
}

/// Entering the rotation leg through the clock-expired-429 unmask must not
/// cost the endpoint-level backoff when the refresh can't complete: the bail
/// keeps `RateLimited` plus the server hint, while a 401-entered bail stays
/// `Cached`.
#[test]
fn failed_unmask_bail_keeps_the_429_context() {
    use std::time::Duration;

    use super::{FetchStatus, rotation_bail_context};

    // 429-entered with a server hint: both survive the failed refresh.
    let (status, retry_after) = rotation_bail_context(Some(Some(Duration::from_secs(30))));
    assert_eq!(status, FetchStatus::RateLimited);
    assert_eq!(retry_after, Some(Duration::from_secs(30)));

    // 429-entered without a hint: still RateLimited so the no-hint ladder runs.
    let (status, retry_after) = rotation_bail_context(Some(None));
    assert_eq!(status, FetchStatus::RateLimited);
    assert_eq!(retry_after, None);

    // 401-entered: plain cached bail, no phantom rate limit.
    let (status, retry_after) = rotation_bail_context(None);
    assert_eq!(status, FetchStatus::Cached);
    assert_eq!(retry_after, None);
}

/// The unmask-bail outcome drives the same deferral + streak accounting as a
/// plain 429: the next slot lands on `now + retry_after` and the consecutive
/// count survives the failed refresh attempt.
#[test]
fn failed_unmask_outcome_defers_and_streaks_like_a_429() {
    use std::time::Duration;

    use super::{
        FetchOutcome, StatusStore, apply_outcome, now_ms, partition_due, rotation_bail_context,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let statuses: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let (status, retry_after) = rotation_bail_context(Some(Some(Duration::from_secs(300))));
    let outcome = FetchOutcome {
        name: crate::profile::ProfileName::from("u"),
        info: None,
        status,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after,
    };

    let before = now_ms();
    apply_outcome(
        outcome,
        &store,
        &statuses,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();

    assert_eq!(
        streaks.lock().unwrap().get("u").map(|c| c.rate_limit),
        Some(1),
        "the failed unmask still counts toward the 429 streak"
    );

    let extra = 300_000 - REFRESH_INTERVAL_MS;
    let stamp = last_fetched
        .lock()
        .unwrap()
        .get("u")
        .copied()
        .expect("stamp present")
        .as_millis();
    assert!(
        (before + extra..=after + extra).contains(&stamp),
        "deferred stamp must sit retry_after - interval ahead of now"
    );

    // partition_due honors the deferral end to end.
    let snapshot = vec![token("u")];
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let (due, _) = partition_due(
        &snapshot,
        stamp + REFRESH_INTERVAL_MS - 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert!(due.is_empty(), "not due before the deferred slot");
    let (due, _) = partition_due(
        &snapshot,
        stamp + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert_eq!(due.len(), 1, "due once the deferred slot arrives");
}

/// A forced refetch marks `Queued`; if no leg schedules that name this tick (its
/// profile vanished from both snapshots), the orphan sweep clears it so the
/// spinner can't freeze — but a name that IS scheduled, and one mid-`Refreshing`,
/// are both left alone.
#[test]
fn orphaned_forced_cleared_but_scheduled_and_refreshing_kept() {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    mark_activity(
        &activity,
        &crate::profile::ProfileName::from("orphan"),
        ProfileActivity::Queued,
    );
    mark_activity(
        &activity,
        &crate::profile::ProfileName::from("scheduled"),
        ProfileActivity::Queued,
    );
    mark_activity(
        &activity,
        &crate::profile::ProfileName::from("rotating"),
        ProfileActivity::Refreshing,
    );

    let forced: HashSet<String> = ["orphan", "scheduled", "rotating"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let scheduled: HashSet<String> = ["scheduled"].iter().map(|s| s.to_string()).collect();

    clear_orphaned_forced(&activity, &forced, &scheduled);

    let a = activity.lock().unwrap();
    assert!(!a.contains_key("orphan"), "orphaned forced name is cleared");
    assert_eq!(
        a.get("scheduled").copied(),
        Some(ProfileActivity::Queued),
        "a scheduled name keeps its mark"
    );
    assert_eq!(
        a.get("rotating").copied(),
        Some(ProfileActivity::Refreshing),
        "a refreshing name is owned by the rotate worker, left alone"
    );
}

// ── Panic-clear discipline ────────────────────────────────────────────────────

/// The scheduler tick's mark/join/clear discipline must clear the ActivityStore
/// slot even when a fetch worker panics — exercises the `Err(_)` arm of
/// `h.join()` without real HTTP or a full scheduler.
#[test]
fn activity_cleared_on_worker_panic() {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let name = "test-profile";

    mark_activity(
        &activity,
        &crate::profile::ProfileName::from(name),
        ProfileActivity::Fetching,
    );
    assert!(
        !activity.lock().unwrap().is_empty(),
        "slot must be set after mark_activity"
    );

    let h = std::thread::spawn(|| -> () { panic!("simulated worker panic") });

    // join loop Err arm: clear slot on panic
    match h.join() {
        Ok(_) => panic!("expected panic in worker"),
        Err(_) => clear_activity(&activity, &crate::profile::ProfileName::from(name)),
    }

    assert!(
        activity.lock().unwrap().is_empty(),
        "activity slot must be cleared after worker panic"
    );
}

/// A disk-cache fallback (`from_fetch: false`) must not clobber a newer store
/// entry: while `/usage` rate-limits, every tick recycles the stale on-disk
/// snapshot, and treating it as fresh froze the UI + auto-start scan on
/// pre-kick windowless data. Regression for the RateLimited-masking bug.
#[test]
fn cached_fallback_does_not_clobber_store() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome};
    use crate::usage::{UsageInfo, UsageWindow};

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let live = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 1.0,
            resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
        }),
        ..Default::default()
    };
    store.lock().unwrap().insert("a".to_string(), live);

    let stale_windowless = UsageInfo::default();
    apply_outcome(
        FetchOutcome {
            name: crate::profile::ProfileName::from("a"),
            info: Some(stale_windowless.clone()),
            status: FetchStatus::RateLimited,
            rotated: None,
            from_fetch: false,
            refresh_failed: false,
            plan_override: None,
            retry_after: None,
        },
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    assert!(
        store.lock().unwrap().get("a").unwrap().five_hour.is_some(),
        "a cache fallback must not overwrite a newer store entry"
    );
    assert_eq!(
        status.lock().unwrap().get("a").copied(),
        Some(FetchStatus::RateLimited),
        "the RateLimited status still surfaces"
    );

    // Cold start: the same fallback DOES fill an absent entry.
    apply_outcome(
        FetchOutcome {
            name: crate::profile::ProfileName::from("b"),
            info: Some(stale_windowless),
            status: FetchStatus::Cached,
            rotated: None,
            from_fetch: false,
            refresh_failed: false,
            plan_override: None,
            retry_after: None,
        },
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    assert!(
        store.lock().unwrap().contains_key("b"),
        "a cache fallback still cold-fills an absent entry"
    );
}

/// The scheduler half of the /usage-429 decouple: a `/profile` plan fetched
/// despite the 429 rides the cached bail and advances the STORED tier (Pro →
/// Free/canceled) while the cached 5h window is preserved, and the overlay
/// reaches disk so CLI/MCP readers see it too. The 429 status still surfaces.
#[test]
fn cached_bail_overlays_a_fresh_plan_onto_store_and_disk() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome};
    use crate::usage::{PlanInfo, PlanTier, UsageInfo, UsageWindow};

    let _home = crate::testutil::HomeSandbox::new();
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    // Prior state: a live 5h window under a (now stale) Pro tier, in both the
    // store and the disk cache the bail loads from.
    let prior = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 5.0,
            resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
        }),
        plan: Some(PlanInfo {
            tier: PlanTier::Pro,
            subscription_status: None,
        }),
        ..Default::default()
    };
    store.lock().unwrap().insert("a".to_string(), prior.clone());
    crate::testutil::register_names(&["a"]);
    super::write_profile_cache(
        &crate::profile::ProfileName::from("a"),
        super::USAGE_CACHE_FILE,
        &prior,
    );

    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };
    apply_outcome(
        FetchOutcome::cached(
            &crate::profile::ProfileName::from("a"),
            FetchStatus::RateLimited,
            None,
            None,
        )
        .with_plan(Some(canceled)),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );

    let got = store.lock().unwrap().get("a").cloned().unwrap();
    let plan = got.plan.as_ref().unwrap();
    assert_eq!(plan.tier, PlanTier::Free, "the stored tier flips to Free");
    assert!(
        plan.is_canceled(),
        "the canceled state persists to the store"
    );
    assert!(
        got.five_hour.is_some(),
        "the cached 5h window is preserved — only the tier advanced"
    );
    assert_eq!(
        status.lock().unwrap().get("a").copied(),
        Some(FetchStatus::RateLimited),
        "the account stays visibly rate-limited"
    );

    let disk = super::load_profile_cache::<UsageInfo>(
        &crate::profile::ProfileName::from("a"),
        super::USAGE_CACHE_FILE,
    )
    .unwrap();
    assert!(
        disk.plan.unwrap().is_canceled(),
        "the flip persists to usage_cache.json for CLI/MCP readers"
    );
}

/// The cold-canceled class: a profile added while ALREADY canceled 429s `/usage`
/// from its first poll and has no `usage_cache.json`, so the cached bail carries
/// a plan but `info=None`. The plan must still be recorded — on a windowless,
/// plan-only entry — in BOTH the store and disk, or the cancellation is dropped
/// every tick and the dead account stays selectable by the fallback walk.
#[test]
fn cold_bail_records_a_plan_only_canceled_entry() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome};
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let _home = crate::testutil::HomeSandbox::new();
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    // No prior store entry and no usage_cache.json: `cached()` yields info=None.
    crate::testutil::register_names(&["cold"]);
    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };
    apply_outcome(
        FetchOutcome::cached(
            &crate::profile::ProfileName::from("cold"),
            FetchStatus::RateLimited,
            None,
            None,
        )
        .with_plan(Some(canceled)),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );

    let got = store.lock().unwrap().get("cold").cloned();
    assert!(
        got.as_ref()
            .and_then(|i| i.plan.as_ref())
            .is_some_and(|p| p.is_canceled()),
        "the store records the canceled plan even with no prior snapshot"
    );
    assert!(
        got.unwrap().five_hour.is_none(),
        "a plan-only entry — no windows to show"
    );

    let disk = super::load_profile_cache::<UsageInfo>(
        &crate::profile::ProfileName::from("cold"),
        super::USAGE_CACHE_FILE,
    );
    assert!(
        disk.and_then(|i| i.plan).is_some_and(|p| p.is_canceled()),
        "and persists to usage_cache.json so the flip survives and readers see it"
    );
}

/// `mark_window_open` synthesizes a live 5h window after a successful kick
/// (the kick's 200 IS the window opening; /usage may 429 for minutes), but
/// never touches a window that is already live.
#[test]
fn mark_window_open_synthesizes_only_when_not_live() {
    use super::mark_window_open;
    use crate::usage::{UsageInfo, UsageWindow, iso_to_epoch_secs};

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let now = 1_780_000_000i64;

    // Absent entry → synthetic window resets now + 5h.
    mark_window_open(&store, &crate::profile::ProfileName::from("a"), now);
    let resets = store.lock().unwrap()["a"]
        .five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs);
    assert_eq!(
        resets,
        Some(now + 5 * 3600),
        "synthetic window opens at +5h"
    );

    // Live window → untouched (kick into a live window must not extend it).
    let live_resets = "2999-01-01T00:00:00+00:00";
    store.lock().unwrap().insert(
        "b".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 42.0,
                resets_at: Some(live_resets.to_string()),
            }),
            ..Default::default()
        },
    );
    mark_window_open(&store, &crate::profile::ProfileName::from("b"), now);
    let kept = store.lock().unwrap()["b"].five_hour.clone().unwrap();
    assert_eq!(kept.resets_at.as_deref(), Some(live_resets));
    assert_eq!(kept.utilization, 42.0);

    // Expired window → replaced by a fresh synthetic one.
    store.lock().unwrap().insert(
        "c".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 88.0,
                resets_at: Some("2020-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    mark_window_open(&store, &crate::profile::ProfileName::from("c"), now);
    let replaced = store.lock().unwrap()["c"].five_hour.clone().unwrap();
    assert_eq!(
        replaced.resets_at.as_deref().and_then(iso_to_epoch_secs),
        Some(now + 5 * 3600)
    );
    assert_eq!(replaced.utilization, 0.0, "fresh window starts at zero");
}

/// `window_lapsed` gates the auto-start kick: an absent store entry (never
/// fetched this run) is NOT lapsed — fetch first, kick next tick — while a
/// fetched entry with no 5h window or a past `resets_at` IS lapsed, and a future
/// `resets_at` is live.
#[test]
fn window_lapsed_only_fires_on_a_fetched_expired_window() {
    use super::UsageStore;
    use crate::usage::{UsageInfo, UsageWindow};

    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let now = 1_780_000_000i64;

    // Never fetched (absent) → not lapsed: fetch first.
    assert!(
        !window_lapsed(&store, &crate::profile::ProfileName::from("a"), now),
        "an absent entry must not kick — fetch first, kick next tick"
    );

    // Fetched, no 5h window present → lapsed.
    store
        .lock()
        .unwrap()
        .insert("a".to_string(), UsageInfo::default());
    assert!(
        window_lapsed(&store, &crate::profile::ProfileName::from("a"), now),
        "a fetched entry with no live window is lapsed"
    );

    // Past resets_at → lapsed.
    store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some("2020-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    assert!(
        window_lapsed(&store, &crate::profile::ProfileName::from("a"), now),
        "a past resets_at is lapsed"
    );

    // Future resets_at → live, not lapsed.
    store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    assert!(
        !window_lapsed(&store, &crate::profile::ProfileName::from("a"), now),
        "a future resets_at is a live window — no kick"
    );
}

/// The auto-start kick's firing rules: never mid-`/usage`-429-streak; a lapsed
/// window opens on the kick's backoff cadence; a live window re-tests a standing
/// block on the poll cadence (recovery may be imminent). Mid-streak the kick is
/// suppressed so it can't re-hit (and prolong) a throttled endpoint every slot; a
/// live `/usage` body clears the streak and the next due tick kicks cleanly.
#[test]
fn kick_suppressed_during_rate_limit_streak() {
    use super::should_open_window;

    // args: (streak, window_lapsed, kick_due, has_block, queue_due)
    assert!(
        should_open_window(0, true, true, false, true),
        "lapsed + no streak → open"
    );
    assert!(
        !should_open_window(1, true, true, false, true),
        "lapsed but 429-streaking → suppress the kick"
    );
    assert!(
        !should_open_window(5, true, true, false, true),
        "deep streak → still suppressed"
    );
    assert!(
        !should_open_window(0, false, true, false, true),
        "a live window with no block never kicks"
    );
    assert!(
        should_open_window(0, false, true, true, true),
        "a live window WITH a standing block re-tests it — the window can be a \
         Claude-web open while Claude Code stays 429'd, so only a landed kick \
         proves the block is gone"
    );
    assert!(
        should_open_window(0, false, false, true, true),
        "a live-window block re-tests on the POLL cadence, not the deep kick \
         backoff — the window reopened (maybe via web), so recovery may be \
         imminent and we must not wait out the ~15min ladder"
    );
    assert!(
        !should_open_window(1, false, false, true, true),
        "but a /usage 429-streak still suppresses even the live-window re-test"
    );
    assert!(
        !should_open_window(0, true, false, true, true),
        "a LAPSED-window kick-429 block whose retry isn't due still waits its \
         backoff — no reopened-window signal, so don't re-hit a dead endpoint"
    );
    assert!(
        !should_open_window(0, true, true, false, false),
        "the queue gate holds the LAPSED leg: an unelected member with a \
         lapsed window and a due kick clock still may not open"
    );
    assert!(
        should_open_window(0, false, true, true, false),
        "…and only the lapsed leg: the live-window re-test is a health probe \
         the queue must never delay"
    );
}

// The `run_fetch` wiring seam: a LIVE 5h window with a standing block must
// re-test (the fix), a healthy live window stays quiet. Guards the
// `block.is_some()` → `has_block` plumbing `should_open_window`'s own test can't
// reach, since `run_fetch` is HTTP-bound.
#[test]
fn auto_start_re_tests_a_live_window_block_but_leaves_a_healthy_one() {
    use super::{KickBlock, KickBlocks, PollStreaks, auto_start_should_kick};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso};

    let now = 3_000_000;
    let streaks: PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let live_store = || -> UsageStore {
        Arc::new(RankedMutex::new(HashMap::from([(
            "a".to_string(),
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: 5.0,
                    resets_at: Some(epoch_secs_to_iso(now + 3600)),
                }),
                ..Default::default()
            },
        )])))
    };

    let blocked: KickBlocks = Arc::new(RankedMutex::new(HashMap::from([(
        "a".to_string(),
        KickBlock {
            streak: 3,
            rejected: true,
            until: Some(now + 900),
            next_retry: now + 600,
        },
    )])));
    assert!(
        auto_start_should_kick(
            &streaks,
            &live_store(),
            &blocked,
            &crate::profile::ProfileName::from("a"),
            now,
            true
        ),
        "a live window with a standing block re-tests it — the fix"
    );

    let clean: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    assert!(
        !auto_start_should_kick(
            &streaks,
            &live_store(),
            &clean,
            &crate::profile::ProfileName::from("a"),
            now,
            true
        ),
        "a healthy live window with no block must not kick"
    );
}

/// The kick-429 block's retry clock: the streak climbs the shared backoff
/// ladder but never schedules past the limiter's advertised ceiling, and a
/// passed ceiling (or no block at all) is always due.
#[test]
fn kick_block_backoff_decays_toward_the_advertised_ceiling() {
    use super::{KickBlock, kick_block_after_429, kick_retry_due};
    use crate::oauth::KickRateLimit;

    let now = 1_000_000;
    let rl = KickRateLimit {
        rejected: true,
        until_epoch_secs: Some(now + 10_000),
    };

    let first = kick_block_after_429(None, &rl, now);
    assert_eq!(first.streak, 1);
    assert!(first.rejected);
    assert_eq!(first.until, Some(now + 10_000));
    assert_eq!(
        first.next_retry,
        now + 10,
        "streak 1 rides the ladder base (10s), far below the ceiling"
    );
    assert!(
        !kick_retry_due(Some(&first), now + 5),
        "before next_retry → not due"
    );
    assert!(
        kick_retry_due(Some(&first), now + 10),
        "at next_retry → due"
    );
    assert!(kick_retry_due(None, now), "no block → always due");

    // Climb the ladder deep enough that it would overshoot a near ceiling.
    let deep = kick_block_after_429(Some(first), &rl, now + 9_990);
    assert_eq!(deep.streak, 2);
    let near_rl = KickRateLimit {
        rejected: true,
        until_epoch_secs: Some(now + 9_995),
    };
    let clamped = kick_block_after_429(Some(deep), &near_rl, now + 9_990);
    assert_eq!(
        clamped.next_retry,
        now + 9_995,
        "ladder overshooting the advertised ceiling clamps to the ceiling — \
         the reset is an upper bound the retry must reach, never sleep past"
    );

    // No headers at all still blocks, on the pure ladder.
    let bare = KickRateLimit {
        rejected: false,
        until_epoch_secs: None,
    };
    let no_hint = kick_block_after_429(None, &bare, now);
    assert!(!no_hint.rejected);
    assert_eq!(no_hint.until, None);
    assert_eq!(no_hint.next_retry, now + 10);

    // With NO ceiling to clamp to, a deep streak must still cap at the shared
    // 15min MAX_RETRY_AFTER_MS — an uncapped ladder (~6h at streak 8) would
    // wedge the window closed for hours after a header-less outage clears.
    let deep_bare = kick_block_after_429(
        Some(KickBlock {
            streak: 8,
            rejected: false,
            until: None,
            next_retry: now,
        }),
        &bare,
        now,
    );
    assert!(
        deep_bare.next_retry <= now + 15 * 60,
        "ladder must cap at 15min, got +{}s",
        deep_bare.next_retry - now
    );
}

/// Only a switch-grade block moves the fallback chain: the limiter's own
/// `rejected` verdict, ≥2 consecutive kicks, ceiling still ahead. Anything
/// weaker gets the pill + backoff but never rotates accounts.
#[test]
fn only_a_switch_grade_kick_block_rotates_the_chain() {
    use super::{KickBlock, KickBlocks, kick_block_switch_grade, kick_rejected_names};

    let now = 3_000_000;
    let grade = KickBlock {
        streak: 2,
        rejected: true,
        until: Some(now + 600),
        next_retry: now + 30,
    };
    assert!(kick_block_switch_grade(&grade, now));
    assert!(
        !kick_block_switch_grade(&KickBlock { streak: 1, ..grade }, now),
        "one 429 must not move the chain — flap guard"
    );
    assert!(
        !kick_block_switch_grade(
            &KickBlock {
                rejected: false,
                ..grade
            },
            now
        ),
        "a burst 429 without the limiter's rejected verdict must not move the chain"
    );
    assert!(
        !kick_block_switch_grade(
            &KickBlock {
                until: None,
                ..grade
            },
            now
        ),
        "no advertised ceiling → no switch-grade claim"
    );
    assert!(
        !kick_block_switch_grade(&grade, now + 601),
        "a passed ceiling ends the claim — the next kick re-proves it or clears"
    );

    let blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::from([
        ("dead".to_string(), grade),
        ("blip".to_string(), KickBlock { streak: 1, ..grade }),
    ])));
    assert_eq!(
        kick_rejected_names(&blocks, now),
        vec![crate::profile::ProfileName::from("dead")]
    );
}

/// `note_kick_outcome` lifecycle: a 429 upserts the block and writes the
/// per-profile cache file; a later successful kick clears both. A no-metadata
/// failure (transport, 401 path) leaves existing state untouched.
#[test]
fn kick_block_persists_and_clears_by_outcome() {
    use super::{kick_block, note_kick_outcome, sync_kick_blocks_from_cache};
    use crate::oauth::KickRateLimit;
    use crate::profile_cache::{KICK_BLOCK_CACHE_FILE, load_profile_cache};

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["kitty"]);
    let blocks: super::KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let now = 2_000_000;
    let rl = KickRateLimit {
        rejected: true,
        until_epoch_secs: Some(now + 600),
    };

    note_kick_outcome(
        &blocks,
        &crate::profile::ProfileName::from("kitty"),
        false,
        Some(rl),
        now,
    );
    let live = kick_block(&blocks, &crate::profile::ProfileName::from("kitty"))
        .expect("429 outcome must block");
    assert_eq!(live.streak, 1);
    let on_disk: super::KickBlock = load_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        KICK_BLOCK_CACHE_FILE,
    )
    .expect("block written through");
    assert_eq!(on_disk, live);

    // A failure with no limiter metadata must not disturb the block.
    note_kick_outcome(
        &blocks,
        &crate::profile::ProfileName::from("kitty"),
        false,
        None,
        now + 20,
    );
    assert_eq!(
        kick_block(&blocks, &crate::profile::ProfileName::from("kitty")),
        Some(live)
    );

    // A second 429 grows the streak in place.
    note_kick_outcome(
        &blocks,
        &crate::profile::ProfileName::from("kitty"),
        false,
        Some(rl),
        now + 30,
    );
    assert_eq!(
        kick_block(&blocks, &crate::profile::ProfileName::from("kitty")).map(|b| b.streak),
        Some(2)
    );

    // A fresh map (new process) resumes the persisted block…
    let rehydrated: super::KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    sync_kick_blocks_from_cache(&rehydrated, &["kitty".to_string()]);
    assert_eq!(
        kick_block(&rehydrated, &crate::profile::ProfileName::from("kitty")).map(|b| b.streak),
        Some(2)
    );

    // …and a successful kick clears map + file, so the next sync clears mirrors.
    note_kick_outcome(
        &blocks,
        &crate::profile::ProfileName::from("kitty"),
        true,
        None,
        now + 40,
    );
    assert_eq!(
        kick_block(&blocks, &crate::profile::ProfileName::from("kitty")),
        None
    );
    assert!(
        load_profile_cache::<super::KickBlock>(
            &crate::profile::ProfileName::from("kitty"),
            KICK_BLOCK_CACHE_FILE
        )
        .is_none(),
        "clearing must remove the cache file"
    );
    sync_kick_blocks_from_cache(&rehydrated, &["kitty".to_string()]);
    assert_eq!(
        kick_block(&rehydrated, &crate::profile::ProfileName::from("kitty")),
        None,
        "a mirroring instance drops the block once the file is gone"
    );
}

/// Auto-switch and recovery decisions act only on a confirmed-live (`Fresh`)
/// read. A `Cached` window may have rolled over and a `RateLimited` one may be a
/// synthetic just-kicked 0% — both must be treated as undecidable, as must a
/// profile with no read yet.
#[test]
fn only_a_fresh_read_drives_a_switch_decision() {
    use super::{FetchStatus, StatusStore, decision_fresh};

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    {
        let mut s = status.lock().unwrap();
        s.insert("fresh".to_string(), FetchStatus::Fresh);
        s.insert("cached".to_string(), FetchStatus::Cached);
        s.insert("limited".to_string(), FetchStatus::RateLimited);
        s.insert("failed".to_string(), FetchStatus::Failed);
    }

    assert!(decision_fresh(
        &status,
        &crate::profile::ProfileName::from("fresh")
    ));
    assert!(
        !decision_fresh(&status, &crate::profile::ProfileName::from("cached")),
        "a possibly rolled-over cached window must not drive a switch"
    );
    assert!(
        !decision_fresh(&status, &crate::profile::ProfileName::from("limited")),
        "a synthetic rate-limited window must not drive a switch"
    );
    assert!(!decision_fresh(
        &status,
        &crate::profile::ProfileName::from("failed")
    ));
    assert!(
        !decision_fresh(&status, &crate::profile::ProfileName::from("absent")),
        "no read yet → no decision"
    );
}

/// AUTH-4: `scan_auto_switch` bypasses the freshness gate for an auth-broken
/// active — its reads can never be `Fresh` again (the login is dead), so
/// requiring one froze the scan forever and wedged the daemon on the dead
/// account while a viable sibling idled (observed live 2026-07-09). A healthy
/// active keeps the gate: the same frozen store state must NOT drive a switch
/// when the account is merely stale, only when it is confirmed dead.
#[test]
fn scan_auto_switch_walks_off_a_broken_active_without_a_fresh_read() {
    use super::{FetchStatus, PendingSwitch, PendingSwitchOff, StatusStore, scan_auto_switch};
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    let frozen_state = || {
        // The wedge's exact shape: the active's last-ever read is maxed on a
        // window that has since lapsed (reads as idle headroom), status stuck
        // on RateLimited; the sibling is genuinely viable and Fresh.
        let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([
            (
                "a".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: 100.0,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() - 3600)),
                    }),
                    ..Default::default()
                },
            ),
            (
                "b".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: 10.0,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
                    }),
                    ..Default::default()
                },
            ),
        ])));
        let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
            ("a".to_string(), FetchStatus::RateLimited),
            ("b".to_string(), FetchStatus::Fresh),
        ])));
        let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        (store, status, streaks, activity, pending, pending_off)
    };
    let config_handle = |broken: bool| -> crate::profile::ConfigHandle {
        let mut cfg = AppConfig {
            state: AppState {
                active_profile: Some("a".into()),
                profiles: vec!["a".into(), "b".into()],
                fallback_chain: vec!["a".into(), "b".into()],
                ..AppState::default()
            },
            profiles: vec![
                Profile::new("a".to_string(), None, None),
                Profile::new("b".to_string(), None, None),
            ],
        };
        cfg.set_auth_broken(&crate::profile::ProfileName::from("a"), broken);
        Arc::new(RankedMutex::new(cfg))
    };

    // Broken active → the gate is bypassed and the walk queues the sibling.
    let (store, status, streaks, activity, pending, pending_off) = frozen_state();
    scan_auto_switch(
        &config_handle(true),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &streaks,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &activity,
        &pending,
        &pending_off,
    );
    assert!(
        pending.lock().unwrap().contains("b"),
        "a dead active must be walked away from without waiting for a Fresh read"
    );

    // Healthy active, identical frozen stores (lapsed window = headroom, shallow
    // streak) → the freshness gate holds: not broken, not stuck-RL, not Fresh.
    let (store, status, streaks, activity, pending, pending_off) = frozen_state();
    scan_auto_switch(
        &config_handle(false),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &streaks,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &activity,
        &pending,
        &pending_off,
    );
    assert!(
        pending.lock().unwrap().is_empty(),
        "a merely-stale healthy active must still not drive a switch"
    );
}

/// The scan fills `ChainSnapshot::fresh`, which `snapshot_chain` cannot: config
/// carries no freshness and `Profile.fetch_status` is written by the UI thread
/// only, so the daemon reads it stale. Without this fill the store twin's
/// fresh-preference pass matches nothing and silently degrades to walk order.
#[test]
fn scan_auto_switch_prefers_a_fresh_member_over_an_earlier_stale_one() {
    use super::{FetchStatus, PendingSwitch, StatusStore, scan_auto_switch};
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    let live = |utilization: f64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        ..Default::default()
    };
    // Spent active; both siblings read as headroom, but only c's read is live.
    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), live(100.0)),
        ("b".to_string(), live(10.0)),
        ("c".to_string(), live(20.0)),
    ])));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), FetchStatus::Fresh),
        ("b".to_string(), FetchStatus::Cached),
        ("c".to_string(), FetchStatus::Fresh),
    ])));
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into(), "c".into()],
            fallback_chain: vec!["a".into(), "b".into(), "c".into()],
            ..AppState::default()
        },
        profiles: vec![
            Profile::new("a".to_string(), None, None),
            Profile::new("b".to_string(), None, None),
            Profile::new("c".to_string(), None, None),
        ],
    }));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
    scan_auto_switch(
        &config,
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &pending,
        &Arc::new(RankedMutex::new(false)),
    );
    let queued = pending.lock().unwrap();
    assert!(
        queued.contains("c"),
        "the scan must fill `fresh` so the walk prefers c's trusted read; queued: {:?}",
        *queued
    );
    assert!(
        !queued.contains("b"),
        "b is reached first but its read is Cached — walk order must not win"
    );
}

/// `decision_fresh_any` unions BOTH status stores. Before the fix the scheduler
/// twin read only the OAuth `StatusStore`, so a fresh third-party member looked
/// stale to its fresh-preference/recovery gate while the UI twin (reading
/// `Profile.fetch_status`, filled from both in `apply_usage`) saw it — the twins
/// disagreed on a mixed OAuth+third-party chain (2026-07-17).
#[test]
fn decision_fresh_any_reads_both_the_oauth_and_third_party_stores() {
    use super::{FetchStatus, StatusStore, ThirdPartyStatusStore, decision_fresh_any};

    let oauth: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), FetchStatus::Fresh),
        ("stale".to_string(), FetchStatus::Cached),
    ])));
    let tp: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::from([
        ("b".to_string(), FetchStatus::Fresh),
        ("tp-stale".to_string(), FetchStatus::Cached),
    ])));

    assert!(
        decision_fresh_any(&oauth, &tp, &crate::profile::ProfileName::from("a")),
        "OAuth-fresh counts"
    );
    assert!(
        decision_fresh_any(&oauth, &tp, &crate::profile::ProfileName::from("b")),
        "third-party-fresh must count too — the whole point of the fix"
    );
    assert!(
        !decision_fresh_any(&oauth, &tp, &crate::profile::ProfileName::from("stale")),
        "OAuth Cached is not fresh"
    );
    assert!(
        !decision_fresh_any(&oauth, &tp, &crate::profile::ProfileName::from("tp-stale")),
        "third-party Cached is not fresh"
    );
    assert!(
        !decision_fresh_any(&oauth, &tp, &crate::profile::ProfileName::from("unknown")),
        "absent in both stores is not fresh"
    );
}

/// RLS-1 (the RateLimited analogue of AUTH-4): a **deep-slot stuck RateLimited**
/// active bypasses the freshness gate so the daemon stops wedging on a
/// rate-limited account — but, unlike auth-broken, the switch still faces the
/// walk's last-known exhaustion gate. Four cases share one frozen shape:
///   * deep streak (> cap) + genuinely-spent LIVE window → switches away;
///   * deep streak + LIVE headroom → stays (throttle artifact, no false switch);
///   * deep streak + stale-HIGH but LAPSED window → stays — the load-bearing
///     RLS-1↔AUTH-4 asymmetry: this is the exact frozen shape a real 429 storm
///     holds (the last Fresh window is preserved; after ~5h it lapses to
///     `resets_at` in the past). An auth-broken active WALKS AWAY on this same
///     store (it bypasses the exhaustion gate too); a stuck-RL active must NOT,
///     since `five_hour_live` reads the lapsed window as regained headroom. A
///     false switch here would log out every running claude over a reset account;
///   * shallow streak (≤ cap) + spent window → stays (give the active cap's
///     frequent retries a chance to return a Fresh read first).
#[test]
fn scan_auto_switch_distrusts_a_deep_slot_stuck_rate_limited_active() {
    use super::{
        ACTIVE_CAP_MAX_STREAK, FetchStatus, PendingSwitch, PendingSwitchOff, PollStreaks,
        StatusStore, scan_auto_switch,
    };
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    // `a` is active and RateLimited; `active_util` on a 5h window whose reset is
    // `resets_offset` seconds from now (negative = a LAPSED window, which
    // `five_hour_live` reads as regained headroom regardless of `active_util`);
    // `streak` sets slot depth. `b` is a viable Fresh sibling.
    let frozen_state = |active_util: f64, resets_offset: i64, streak: u32| {
        let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([
            (
                "a".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: active_util,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + resets_offset)),
                    }),
                    ..Default::default()
                },
            ),
            (
                "b".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: 10.0,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
                    }),
                    ..Default::default()
                },
            ),
        ])));
        let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
            ("a".to_string(), FetchStatus::RateLimited),
            ("b".to_string(), FetchStatus::Fresh),
        ])));
        let streaks: PollStreaks = Arc::new(RankedMutex::new(HashMap::from([(
            "a".to_string(),
            super::StreakCounts {
                rate_limit: streak,
                refresh_fail: 0,
            },
        )])));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        (store, status, streaks, activity, pending, pending_off)
    };
    let config_handle = || -> crate::profile::ConfigHandle {
        Arc::new(RankedMutex::new(AppConfig {
            state: AppState {
                active_profile: Some("a".into()),
                profiles: vec!["a".into(), "b".into()],
                fallback_chain: vec!["a".into(), "b".into()],
                ..AppState::default()
            },
            profiles: vec![
                Profile::new("a".to_string(), None, None),
                Profile::new("b".to_string(), None, None),
            ],
        }))
    };
    let deep = ACTIVE_CAP_MAX_STREAK + 1;
    // The set of profiles the scan queued a switch to (sorted for determinism).
    let run = |util: f64, resets_offset: i64, streak: u32| -> Vec<String> {
        let (store, status, streaks, activity, pending, pending_off) =
            frozen_state(util, resets_offset, streak);
        scan_auto_switch(
            &config_handle(),
            &store,
            &status,
            &Arc::new(RankedMutex::new(HashMap::new())),
            &streaks,
            &Arc::new(RankedMutex::new(HashMap::new())),
            &activity,
            &pending,
            &pending_off,
        );
        let mut queued: Vec<String> = pending.lock().unwrap().iter().cloned().collect();
        queued.sort();
        queued
    };

    // Deep slot + genuinely spent (LIVE window ≥ threshold) → the wedge breaks.
    assert_eq!(
        run(100.0, 3600, deep),
        vec!["b".to_string()],
        "a deep-slot stuck RateLimited active that is genuinely spent must be walked away from"
    );
    // Deep slot but real LIVE headroom → no false switch (the walk's exhaustion
    // gate still holds; distrusting the STATUS never means trusting spent NUMBERS).
    assert!(
        run(10.0, 3600, deep).is_empty(),
        "a stuck RateLimited active with last-known headroom must stay put"
    );
    // Deep slot + stale-HIGH but LAPSED window (the real post-storm shape) → STAY.
    // This is where RLS-1 diverges from AUTH-4: a broken active walks away on this
    // identical store, a stuck-RL one must not — `five_hour_live` reads the lapsed
    // window as regained headroom, so the account is NOT exhausted.
    assert!(
        run(100.0, -3600, deep).is_empty(),
        "a stuck RateLimited active whose maxed window has since LAPSED must stay put \
         (regained headroom), never false-switch off a reset account"
    );
    // Shallow slot + spent → still gated on Fresh; the active cap's frequent
    // retries get a chance to return a live read before we distrust.
    assert!(
        run(100.0, 3600, ACTIVE_CAP_MAX_STREAK).is_empty(),
        "a shallow RateLimited active must still wait for a Fresh read"
    );
}

// ── per-session decision leg ─────────────────────────────────────────────────

/// One registry row, opted IN and shared-flavor. Tests flip the one field they
/// are about, so every other value is pinned here rather than per test.
fn session_row(session_id: &str, start_profile: &str) -> crate::live_sessions::LiveSession {
    crate::live_sessions::LiveSession {
        session_id: session_id.to_string(),
        start_profile: start_profile.to_string(),
        pid: 4242,
        started_at: 1_700_000_000_000,
        cwd: None,
        isolated: false,
        follows_chain: true,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
    }
}

/// File `row` AND hold the liveness marker the decision leg probes, so the row
/// reads live exactly as a real session's does. The returned lock is the fixture:
/// dropping it makes the session dead.
#[must_use]
fn register_live_row(row: &crate::live_sessions::LiveSession) -> std::fs::File {
    crate::live_sessions::register(row).expect("register row");
    let probe = row.current_member.as_deref().unwrap_or(&row.start_profile);
    crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from(probe),
        row.isolated,
        &row.session_id,
    )
    .expect("hold the row's liveness marker")
}

/// A chain of plain OAuth members, every one `swap_eligible`, with `active` as the
/// global active profile.
fn session_config(names: &[&str], active: Option<&str>) -> crate::profile::ConfigHandle {
    use crate::profile::{AppConfig, AppState, Profile};
    Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names.iter().map(|n| (*n).into()).collect(),
            fallback_chain: names.iter().map(|n| (*n).into()).collect(),
            ..AppState::default()
        },
        profiles: names
            .iter()
            .map(|n| Profile::new((*n).to_string(), None, None))
            .collect(),
    }))
}

/// Live 5h windows at the given utilizations — a member at 100 is spent, one at 10
/// has headroom.
fn session_store(utils: &[(&str, f64)]) -> UsageStore {
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    Arc::new(RankedMutex::new(
        utils
            .iter()
            .map(|(name, util)| {
                (
                    (*name).to_string(),
                    UsageInfo {
                        five_hour: Some(UsageWindow {
                            utilization: *util,
                            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
                        }),
                        ..Default::default()
                    },
                )
            })
            .collect(),
    ))
}

fn all_fresh(names: &[&str]) -> super::StatusStore {
    Arc::new(RankedMutex::new(
        names
            .iter()
            .map(|n| ((*n).to_string(), super::FetchStatus::Fresh))
            .collect(),
    ))
}

/// Drive the decision leg with empty third-party / streak / kick state — the
/// inputs most tests in this block do not vary.
fn scan_sessions(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &super::StatusStore,
) {
    scan_sessions_with_streaks(
        config,
        store,
        status,
        &Arc::new(RankedMutex::new(HashMap::new())),
    );
}

/// [`scan_sessions`] with the poll-streak map the freshness gate's stuck-`RateLimited`
/// bypass reads — the one input a deep-slot fixture has to set.
fn scan_sessions_with_streaks(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &super::StatusStore,
    streaks: &super::PollStreaks,
) {
    super::scan_session_switches(
        config,
        store,
        status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        streaks,
        &Arc::new(RankedMutex::new(HashMap::new())),
    );
}

/// The decision written into a row: `(intended_member, chain_cursor)`.
fn decision_of(session_id: &str) -> (Option<String>, Option<usize>) {
    let row = crate::live_sessions::get(session_id).expect("the row must still exist");
    (row.intended_member, row.chain_cursor)
}

/// THE INERTNESS PIN — what makes landing phase 2 before the `--with-fallback`
/// flag safe. Nothing sets `follows_chain` true yet, so an opted-out session must
/// get NO decision even in the state that would move an opted-in one. Without this
/// gate every live session on the box starts following the chain.
#[test]
fn an_opted_out_session_gets_no_decision_so_this_leg_is_inert_until_the_flag_lands() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut row = session_row("4242-0", "a");
    row.follows_chain = false;
    let _marker = register_live_row(&row);

    scan_sessions(
        &session_config(&["a", "b"], Some("a")),
        &session_store(&[("a", 100.0), ("b", 10.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (None, None),
        "an opted-out session must not be pointed at a chain member"
    );
}

/// §9's own phase-2 test: one shared chain, two sessions at different cursors, two
/// different decisions. `a` and `c` are clear, `b` and `d` are spent, so the first
/// clear member AFTER each session's own differs — a leg that decided globally, or
/// off the walk's start rather than the session's, could not produce both.
///
/// Both rows also carry a `start_profile` of `a` with a DIFFERENT
/// `current_member`, so `current_member` winning over `start_profile` is pinned
/// here (its absence is pinned by the never-swapped test below).
#[test]
fn two_opted_in_sessions_on_different_members_get_different_intended_members() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut on_b = session_row("4242-0", "a");
    on_b.current_member = Some("b".to_string());
    let mut on_d = session_row("4242-1", "a");
    on_d.current_member = Some("d".to_string());
    let _b_marker = register_live_row(&on_b);
    let _d_marker = register_live_row(&on_d);

    scan_sessions(
        &session_config(&["a", "b", "c", "d"], Some("a")),
        &session_store(&[("a", 10.0), ("b", 100.0), ("c", 10.0), ("d", 100.0)]),
        &all_fresh(&["a", "b", "c", "d"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("c".to_string()), Some(2)),
        "the session on b must walk forward to c"
    );
    assert_eq!(
        decision_of("4242-1"),
        (Some("a".to_string()), Some(0)),
        "the session on d must WRAP to a — the same chain, a different cursor"
    );
}

/// B6. `current_member` is `None` until a session's first swap, which is every
/// session most of the time. A leg keying on it alone decides nothing for any of
/// them while looking entirely correct.
#[test]
fn a_session_that_has_never_swapped_decides_off_its_start_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = session_row("4242-0", "b");
    assert!(
        row.current_member.is_none(),
        "fixture: the row must be pre-first-swap"
    );
    let _marker = register_live_row(&row);

    scan_sessions(
        &session_config(&["a", "b", "c"], Some("a")),
        &session_store(&[("a", 100.0), ("b", 100.0), ("c", 10.0)]),
        &all_fresh(&["a", "b", "c"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("c".to_string()), Some(2)),
        "a never-swapped session must be decided off the member it launched on"
    );
}

/// B1a. After a wrap-off switch-off-all there is no global active — exactly the
/// state where a session most needs to move. Keying the per-session decision on it
/// leaves every session with no decision, forever and silently.
#[test]
fn a_session_decision_lands_with_no_global_active_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = session_row("4242-0", "a");
    let _marker = register_live_row(&row);

    scan_sessions(
        &session_config(&["a", "b"], None),
        &session_store(&[("a", 100.0), ("b", 10.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "a session with no global active must still be given somewhere to go"
    );
}

/// B1b. A session sitting on a DISABLED member is the one case that must leave it,
/// and the global snapshot drops a disabled non-active member out of the chain —
/// which makes the walk's `position()` return `None` and wedges the session there.
#[test]
fn a_session_on_a_disabled_member_is_moved_off_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = session_row("4242-0", "a");
    let _marker = register_live_row(&row);
    let config = session_config(&["a", "b"], Some("b"));
    config
        .lock()
        .unwrap()
        .find_mut(&crate::profile::ProfileName::from("a"))
        .expect("member a")
        .disabled = true;

    scan_sessions(
        &config,
        &session_store(&[("a", 100.0), ("b", 10.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "a session on a spent, disabled member must be given a way off it"
    );
}

/// B5. A candidate the executor refuses on CONFIG grounds wedges the walk: it is
/// picked every tick, refused every tick, and the session never reaches the member
/// behind it. That kills the chain's recovery half on any mixed chain, and a chain
/// holding a `z.ai` or DeepSeek profile is ordinary.
#[test]
fn a_session_walks_past_a_member_the_executor_would_refuse() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = session_row("4242-0", "a");
    let _marker = register_live_row(&row);
    let config = session_config(&["a", "b", "c"], Some("a"));
    config
        .lock()
        .unwrap()
        .find_mut(&crate::profile::ProfileName::from("b"))
        .expect("member b")
        .base_url = Some("https://api.example/anthropic".into());

    scan_sessions(
        &config,
        &session_store(&[("a", 100.0), ("b", 10.0), ("c", 10.0)]),
        &all_fresh(&["a", "b", "c"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("c".to_string()), Some(2)),
        "b has headroom but a different endpoint, so the walk must reach c"
    );
}

/// B4. `Off` means "sign every account out". A session cannot be left
/// credential-less mid-flight and there is no member name to write, so the row is
/// left exactly as it stands — including an intent a previous tick put there.
#[test]
fn a_wrap_off_decision_leaves_the_sessions_intended_member_untouched() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut row = session_row("4242-0", "a");
    row.intended_member = Some("b".to_string());
    row.chain_cursor = Some(1);
    let _marker = register_live_row(&row);
    let config = session_config(&["a", "b"], Some("a"));
    config.lock().unwrap().state.switch_off_when_spent = true;

    scan_sessions(
        &config,
        &session_store(&[("a", 100.0), ("b", 100.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "a halt decision has no per-session form and must write nothing"
    );
}

/// B7. `gc_stale_runtimes` reaps rows at daemon STARTUP, not per tick, so a
/// SIGKILLed session's row survives the whole daemon run and would keep taking
/// decisions nothing will ever execute.
#[test]
fn a_row_whose_session_is_gone_gets_no_decision() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = session_row("4242-0", "a");
    // Registered, but no marker is ever held — the SIGKILLed shape.
    crate::live_sessions::register(&row).expect("register row");

    scan_sessions(
        &session_config(&["a", "b"], Some("a")),
        &session_store(&[("a", 100.0), ("b", 10.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (None, None),
        "a dead session's row must stop taking decisions"
    );
}

/// B3, mirroring `scan_auto_switch_walks_off_a_broken_active_without_a_fresh_read`
/// one level down: the freshness gate keys on the SESSION's member, not the global
/// active. The same frozen stores must not drive a decision for a member whose
/// reading is merely distrusted, and must drive one once it is confirmed dead.
///
/// The member is GENUINELY spent (a live window at its cap), which is what makes
/// the gate the only thing standing between these stores and a decision. A lapsed
/// window would read as regained headroom and the walk would stay put for its own
/// reasons, leaving the gate unfalsifiable — the mutation caught exactly that.
#[test]
fn a_session_on_a_distrusted_member_gets_no_decision_unless_that_member_is_broken() {
    // `a` is spent on a LIVE window, but its status is stuck on RateLimited with a
    // shallow streak — the reading we do not trust. `b` is viable and Fresh.
    let frozen_store = || session_store(&[("a", 100.0), ("b", 10.0)]);
    let frozen_status = || {
        Arc::new(RankedMutex::new(HashMap::from([
            ("a".to_string(), super::FetchStatus::RateLimited),
            ("b".to_string(), super::FetchStatus::Fresh),
        ])))
    };
    let run = |broken: bool, streak: u32| -> (Option<String>, Option<usize>) {
        let _home = crate::testutil::HomeSandbox::new();
        let _marker = register_live_row(&session_row("4242-0", "a"));
        let config = session_config(&["a", "b"], Some("a"));
        config
            .lock()
            .unwrap()
            .set_auth_broken(&crate::profile::ProfileName::from("a"), broken);
        let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::from([(
            "a".to_string(),
            super::StreakCounts {
                rate_limit: streak,
                refresh_fail: 0,
            },
        )])));
        scan_sessions_with_streaks(&config, &frozen_store(), &frozen_status(), &streaks);
        decision_of("4242-0")
    };

    assert_eq!(
        run(false, 0),
        (None, None),
        "a spent member whose reading is distrusted must not drive a decision"
    );
    assert_eq!(
        run(true, 0),
        (Some("b".to_string()), Some(1)),
        "a member whose login is dead can never read Fresh again, so requiring one \
         wedges the session on it forever"
    );
    // The THIRD rule, and the one the session call site could get wrong on its own:
    // a `RateLimited` reading past the active cap's retry depth is stuck, so no Fresh
    // read is coming for it either. Same stores, same status, deeper slot.
    assert_eq!(
        run(false, super::ACTIVE_CAP_MAX_STREAK + 1),
        (Some("b".to_string()), Some(1)),
        "a deep-slot stuck RateLimited member must be walked off too, or the session \
         wedges on a throttle that never drains"
    );
}

/// B2. `chain_cursor` indexes the ON-DISK chain, never the snapshot's. The
/// snapshot is FILTERED, so with a disabled member ahead of the target the two
/// indices differ — here the target is snapshot slot 1 and config slot 2.
#[test]
fn the_chain_cursor_indexes_the_config_chain_not_the_filtered_snapshot() {
    let _home = crate::testutil::HomeSandbox::new();
    let _marker = register_live_row(&session_row("4242-0", "a"));
    let config = session_config(&["a", "b", "c"], Some("a"));
    config
        .lock()
        .unwrap()
        .find_mut(&crate::profile::ProfileName::from("b"))
        .expect("member b")
        .disabled = true;

    scan_sessions(
        &config,
        &session_store(&[("a", 100.0), ("b", 10.0), ("c", 10.0)]),
        &all_fresh(&["a", "b", "c"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("c".to_string()), Some(2)),
        "the cursor must be c's index in `fallback_chain`, not in the filtered snapshot"
    );
}

/// B9. The batch takes the cross-process state flock ONCE, not once per session.
/// `with_state_lock` is reentrant, so the nested `update_as_daemon` calls take no
/// second flock — per-session acquisition would expose one tick to N × the 25 s
/// `STATE_LOCK_TIMEOUT` instead of one.
#[test]
fn a_batch_of_session_decisions_takes_the_state_flock_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut on_b = session_row("4242-0", "a");
    on_b.current_member = Some("b".to_string());
    let mut on_d = session_row("4242-1", "a");
    on_d.current_member = Some("d".to_string());
    let _b_marker = register_live_row(&on_b);
    let _d_marker = register_live_row(&on_d);

    crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.set(0));
    scan_sessions(
        &session_config(&["a", "b", "c", "d"], Some("a")),
        &session_store(&[("a", 10.0), ("b", 100.0), ("c", 10.0), ("d", 100.0)]),
        &all_fresh(&["a", "b", "c", "d"]),
    );
    let flocks = crate::lock::OUTERMOST_ACQUISITIONS.with(|c| c.get());

    assert_eq!(
        decision_of("4242-0").0,
        Some("c".to_string()),
        "fixture: both sessions must actually be written, or one hold is trivial"
    );
    assert_eq!(decision_of("4242-1").0, Some("a".to_string()));
    assert_eq!(
        flocks, 1,
        "two sessions' decisions must cost one flock wait, not one apiece"
    );
}

/// Recomputation IS this leg's retry — there is deliberately no queue — and both
/// halves of that follow from one rule: the row is written iff it disagrees with
/// the decision. A tick that changes nothing must not take the cross-process state
/// flock to rewrite the same bytes once a second for the life of the session; a row
/// that has drifted from the decision must be corrected on the very next tick, with
/// nothing remembering that the earlier write was lost.
#[test]
fn a_decision_is_rewritten_only_when_the_row_disagrees_with_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let _marker = register_live_row(&session_row("4242-0", "a"));
    let config = session_config(&["a", "b"], Some("a"));
    let store = session_store(&[("a", 100.0), ("b", 10.0)]);
    let status = all_fresh(&["a", "b"]);

    scan_sessions(&config, &store, &status);
    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "fixture: the first tick must decide"
    );

    // The registry keeps one file per session, so the dir holds exactly this row.
    let row_file = std::fs::read_dir(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("live_sessions"),
    )
    .expect("read the registry dir")
    .next()
    .expect("one row on disk")
    .expect("readable entry")
    .path();
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    crate::testutil::set_mtime(&row_file, long_ago);

    scan_sessions(&config, &store, &status);

    assert_eq!(
        std::fs::metadata(&row_file)
            .expect("row metadata")
            .modified()
            .expect("row mtime"),
        long_ago,
        "an unchanged decision must not be rewritten every tick"
    );

    // …and a row that no longer agrees is corrected on the next tick, which is why
    // a dropped write needs no retry of its own.
    crate::live_sessions::update_as_daemon("4242-0", |fields| fields.set_intended_member("a"))
        .expect("drift the row's member");

    scan_sessions(&config, &store, &status);

    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "a row that drifted from the decision must be re-derived and rewritten"
    );

    // The CURSOR is half of the decision, not a derived echo of the name: inserting
    // a member ahead of the target moves its config index while its name stays put,
    // so a guard comparing names alone leaves `chain_cursor` permanently stale —
    // and that field is what the Sessions surface reads.
    crate::live_sessions::update_as_daemon("4242-0", |fields| fields.set_chain_cursor(7))
        .expect("drift the row's cursor");

    scan_sessions(&config, &store, &status);

    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "a row whose cursor drifted must be rewritten even though its member agrees"
    );
}

/// The stay-put `None` — this leg's most common outcome by far, since a session
/// whose member has headroom is the steady state. Nothing may be written for it: a
/// leg that wrote an intent here would touch every live row every tick, and would
/// hand the executor a target for a session that has no reason to move.
#[test]
fn a_healthy_session_gets_no_decision_and_its_row_is_not_touched() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut row = session_row("4242-0", "a");
    // A standing intent from an earlier tick, so "nothing was written" is pinned as
    // the row being UNCHANGED rather than merely still empty.
    row.intended_member = Some("b".to_string());
    row.chain_cursor = Some(1);
    let _marker = register_live_row(&row);

    let row_file = std::fs::read_dir(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("live_sessions"),
    )
    .expect("read the registry dir")
    .next()
    .expect("one row on disk")
    .expect("readable entry")
    .path();
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    crate::testutil::set_mtime(&row_file, long_ago);

    scan_sessions(
        &session_config(&["a", "b"], Some("a")),
        // The session's own member has real headroom, and so does its sibling.
        &session_store(&[("a", 10.0), ("b", 10.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (Some("b".to_string()), Some(1)),
        "a healthy session's row must come through untouched"
    );
    assert_eq!(
        std::fs::metadata(&row_file)
            .expect("row metadata")
            .modified()
            .expect("row mtime"),
        long_ago,
        "a stay-put tick must not write the row at all"
    );
}

/// An `--isolated` session runs a throwaway tree that is deliberately not part of
/// any chain, and the executor refuses it outright. Deciding for one would write an
/// intent nothing can ever act on.
#[test]
fn an_isolated_session_gets_no_decision() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut row = session_row("4242-0", "a");
    row.isolated = true;
    let _marker = register_live_row(&row);

    scan_sessions(
        &session_config(&["a", "b"], Some("a")),
        &session_store(&[("a", 100.0), ("b", 10.0)]),
        &all_fresh(&["a", "b"]),
    );

    assert_eq!(
        decision_of("4242-0"),
        (None, None),
        "an isolated session follows no chain"
    );
}

/// A Fresh `/usage` body fetched in the same tick as a kick can lag the
/// just-opened window and still report it closed; `preserve_live_window` keeps
/// the live window we already hold so it can't re-lapse and re-fire the kick.
/// A body that already carries a live window, or has no live predecessor, is
/// passed through untouched.
#[test]
fn fresh_body_lagging_a_kick_keeps_the_live_window() {
    use super::{five_hour_live, preserve_live_window};
    use crate::usage::{UsageInfo, UsageWindow};

    let now = 1_600_000_000i64; // 2020 — between the two reset stamps below
    let win = |util: f64, resets: &str| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: Some(resets.to_string()),
        }),
        ..Default::default()
    };
    let live = |u| win(u, "2999-01-01T00:00:00+00:00");
    let closed = |u| win(u, "2000-01-01T00:00:00+00:00");

    // Lagging fresh body (closed window) over a just-opened live one → keep live.
    let merged = preserve_live_window(closed(80.0), Some(&live(0.0)), now);
    assert!(
        five_hour_live(&merged, now),
        "a lagging fresh body must not re-close a just-opened window"
    );
    assert_eq!(
        merged.five_hour.unwrap().utilization,
        0.0,
        "keeps the live window verbatim"
    );

    // Fresh body already carries a live window → take it as-is.
    let merged = preserve_live_window(live(12.0), Some(&live(0.0)), now);
    assert_eq!(merged.five_hour.unwrap().utilization, 12.0);

    // Prior window also closed → nothing live to preserve; the fresh body stands.
    let merged = preserve_live_window(closed(80.0), Some(&closed(50.0)), now);
    assert!(!five_hour_live(&merged, now));

    // No prior entry at all → fresh body stands.
    let merged = preserve_live_window(closed(80.0), None, now);
    assert_eq!(merged.five_hour.unwrap().utilization, 80.0);
}

/// A 429's `retry-after` hint defers the profile's next fetch slot: the
/// `last_fetched` stamp lands `retry_after - interval` in the future so
/// `partition_due` marks the profile due (and publishes its countdown) exactly
/// at `now + retry_after`. A 429 with no hint adds a flat 10s beyond the
/// cadence; a zero or sub-interval hint keeps the cadence; an absurd hint clamps
/// to the ceiling.
#[test]
fn retry_after_defers_next_fetch_slot() {
    use std::time::Duration;

    use super::{
        FetchOutcome, FetchStatus, MAX_RETRY_AFTER_MS, RATE_LIMIT_MIN_BACKOFF_MS, StatusStore,
        apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let outcome = |name: &str, retry_after: Option<Duration>| FetchOutcome {
        name: crate::profile::ProfileName::from(name),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after,
    };
    let stamp = |name: &str| {
        last_fetched
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    // retry-after 300s → stamp ≈ now + (300s - interval).
    let before = now_ms();
    apply_outcome(
        outcome("a", Some(Duration::from_secs(300))),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    let extra = 300_000 - REFRESH_INTERVAL_MS;
    let a = stamp("a");
    assert!(
        (before + extra..=after + extra).contains(&a),
        "deferred stamp must sit retry_after - interval ahead of now"
    );
    // partition_due: not due just before now + retry_after, due at it.
    let snapshot = vec![token("a")];
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let (due, next) = partition_due(
        &snapshot,
        a + REFRESH_INTERVAL_MS - 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert!(due.is_empty(), "not due before the deferred slot");
    assert_eq!(
        next.get("a").copied(),
        Some(a + REFRESH_INTERVAL_MS),
        "countdown publishes the deferred slot"
    );
    let (due, _) = partition_due(
        &snapshot,
        a + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
    );
    assert_eq!(due.len(), 1, "due once the deferred slot arrives");

    // No hint → flat 10s backoff beyond the cadence.
    let before = now_ms();
    apply_outcome(
        outcome("b", None),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    let floor = RATE_LIMIT_MIN_BACKOFF_MS;
    assert!(
        (before + floor..=after + floor).contains(&stamp("b")),
        "a 429 with no retry-after defers a flat 10s past now"
    );

    // Hint shorter than the ladder → the ladder wins (max, never suppressed).
    let before = now_ms();
    apply_outcome(
        outcome("c", Some(Duration::from_secs(5))),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    assert!(
        (before + floor..=after + floor).contains(&stamp("c")),
        "a sub-cadence hint cannot undercut the streak ladder"
    );

    // Absurd hint → clamped to the ceiling.
    let before = now_ms();
    apply_outcome(
        outcome("d", Some(Duration::from_secs(86_400))),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    let capped = MAX_RETRY_AFTER_MS - REFRESH_INTERVAL_MS;
    assert!(
        (before + capped..=after + capped).contains(&stamp("d")),
        "huge retry-after clamps to MAX_RETRY_AFTER_MS"
    );

    // Explicit `retry-after: 0` rides the SAME ladder as a missing header.
    // The usage endpoint answers every 429 with `retry-after: 0` while its
    // sliding window counts the rejected requests too — honoring the "retry
    // now" verbatim re-polls at cadence and pins the window full forever
    // (observed 2026-07-11: hours of uninterrupted per-account 429s).
    let before = now_ms();
    apply_outcome(
        outcome("e", Some(Duration::ZERO)),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    assert!(
        (before + floor..=after + floor).contains(&stamp("e")),
        "a zero retry-after must not suppress the backoff ladder"
    );
}

/// Consecutive 429s with no `retry-after` back off exponentially (10s → 30s →
/// 90s past now), and a live fetch resets the streak so the next 429 starts at
/// the base again.
#[test]
fn consecutive_rate_limits_back_off_exponentially() {
    use super::{
        FetchOutcome, FetchStatus, RATE_LIMIT_BACKOFF_FACTOR, RATE_LIMIT_MIN_BACKOFF_MS,
        StatusStore, apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let rate_limited = |from_fetch: bool, status: FetchStatus| FetchOutcome {
        refresh_failed: false,
        plan_override: None,
        name: crate::profile::ProfileName::from("a"),
        info: None,
        status,
        rotated: None,
        from_fetch,
        retry_after: None,
    };
    let stamp = || {
        last_fetched
            .lock()
            .unwrap()
            .get("a")
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    // No retry-after: each consecutive 429 lands the slot one interval + a
    // growing backoff out, i.e. the stamp sits `base * factor^(n-1)` past now.
    // Derived from the constants so retuning the factor can't leave it stale.
    let base = RATE_LIMIT_MIN_BACKOFF_MS;
    let f = RATE_LIMIT_BACKOFF_FACTOR;
    for expect in [base, base * f, base * f * f] {
        let before = now_ms();
        apply_outcome(
            rate_limited(false, FetchStatus::RateLimited),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
            false,
        );
        let after = now_ms();
        assert!(
            (before + expect..=after + expect).contains(&stamp()),
            "consecutive 429 backs off to {expect}ms past now"
        );
    }

    // A live fetch resets the streak (info `None` so no disk write); the next
    // 429 starts at the base backoff again.
    apply_outcome(
        rate_limited(true, FetchStatus::Fresh),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let before = now_ms();
    apply_outcome(
        rate_limited(false, FetchStatus::RateLimited),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    assert!(
        (before + RATE_LIMIT_MIN_BACKOFF_MS..=after + RATE_LIMIT_MIN_BACKOFF_MS).contains(&stamp()),
        "a live fetch resets the backoff streak"
    );
}

/// PR #30 guard: the streak ladder escalates a repeated 429 even while the server
/// hint stays PRESENT. A constant sub-cadence `retry-after` is overridden at every
/// streak by `max(hint, interval + backoff(streak))`, so the same account 429ing
/// three times in a row backs off base → base·f → base·f² just like the no-hint
/// path. Without the `max`, an always-present hint (the real endpoint answers
/// `retry-after: 0`) would freeze the streak counter and pin the account forever.
#[test]
fn hint_present_429s_still_ride_the_streak_ladder() {
    use std::time::Duration;

    use super::{
        FetchOutcome, FetchStatus, RATE_LIMIT_BACKOFF_FACTOR, RATE_LIMIT_MIN_BACKOFF_MS,
        StatusStore, apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    // A constant hint well under the ladder floor — present on every 429, so the
    // escalation below can only come from the streak ladder overriding it.
    let hinted = || FetchOutcome {
        name: crate::profile::ProfileName::from("a"),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: Some(Duration::from_secs(5)),
    };
    let stamp = || {
        last_fetched
            .lock()
            .unwrap()
            .get("a")
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    let base = RATE_LIMIT_MIN_BACKOFF_MS;
    let f = RATE_LIMIT_BACKOFF_FACTOR;
    for expect in [base, base * f, base * f * f] {
        let before = now_ms();
        apply_outcome(
            hinted(),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
            false,
        );
        let after = now_ms();
        assert!(
            (before + expect..=after + expect).contains(&stamp()),
            "a hinted 429 backs off to {expect}ms past now — the ladder, not the 5s hint"
        );
    }
}

/// A transient `Cached`/`Failed` outcome between two 429s must NOT reset the
/// consecutive-429 streak — a network blip mid-storm should leave the ramp
/// climbing (base → base*factor), not drop it back to the base.
#[test]
fn transient_errors_preserve_rate_limit_streak() {
    use super::{
        FetchOutcome, FetchStatus, RATE_LIMIT_BACKOFF_FACTOR, RATE_LIMIT_MIN_BACKOFF_MS,
        StatusStore, apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = |kind: FetchStatus| FetchOutcome {
        name: crate::profile::ProfileName::from("a"),
        info: None,
        status: kind,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: None,
    };
    let apply = |kind: FetchStatus| {
        apply_outcome(
            outcome(kind),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
            false,
        );
    };
    let stamp = || {
        last_fetched
            .lock()
            .unwrap()
            .get("a")
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    // 429 (streak 1), then transient errors that must leave the streak at 1.
    apply(FetchStatus::RateLimited);
    apply(FetchStatus::Cached);
    apply(FetchStatus::Failed);

    // Next 429 → streak 2 (not reset to 1) → base * factor.
    let before = now_ms();
    apply(FetchStatus::RateLimited);
    let after = now_ms();
    let expect = RATE_LIMIT_MIN_BACKOFF_MS * RATE_LIMIT_BACKOFF_FACTOR;
    assert!(
        (before + expect..=after + expect).contains(&stamp()),
        "a Cached/Failed blip must not reset the 429 streak"
    );
}

/// Any on-disk cache seeds at startup as a starting point (store + status +
/// `last_fetched` stamped at the cache mtime so the cadence resumes), regardless of
/// 5h window state. Freshness only picks the status: younger than one interval →
/// `Fresh` (left be), older → `Cached` (refreshed in the background). A missing
/// cache is left for the scheduler.
#[test]
fn try_seed_cache_seeds_any_cache_and_resumes_timer() {
    use std::time::{Duration, SystemTime};

    use super::{FetchStatus, StatusStore, now_ms, try_seed_cache};
    use crate::profile::profile_subpath;
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::testutil::{HomeSandbox, set_mtime};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    let _home = HomeSandbox::new();
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));

    crate::testutil::register_names(&["idle", "stale"]);
    let now_secs = now_epoch_secs();
    let with_reset = |reset_secs: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 12.0,
            resets_at: Some(epoch_secs_to_iso(reset_secs)),
        }),
        ..Default::default()
    };

    // Fresh cache (mtime ~30s ago) whose 5h window already reset (resets_at in the
    // past) — an idle account. Younger than one interval, so seeded `Fresh`.
    write_profile_cache(
        &crate::profile::ProfileName::from("idle"),
        USAGE_CACHE_FILE,
        &with_reset(now_secs - 600),
    );
    let idle_path = profile_subpath(
        &crate::profile::ProfileName::from("idle"),
        "usage_cache.json",
    )
    .expect("idle path");
    set_mtime(&idle_path, SystemTime::now() - Duration::from_secs(30));

    // Stale cache (written 2h ago) whose window is still open — seeded as a starting
    // point with `Cached` status; the scheduler refreshes it in the background.
    write_profile_cache(
        &crate::profile::ProfileName::from("stale"),
        USAGE_CACHE_FILE,
        &with_reset(now_secs + 3600),
    );
    let stale_path = profile_subpath(
        &crate::profile::ProfileName::from("stale"),
        "usage_cache.json",
    )
    .expect("stale path");
    set_mtime(
        &stale_path,
        SystemTime::now() - Duration::from_secs(2 * 3600),
    );

    let now = now_ms();
    assert!(
        try_seed_cache(
            &store,
            &status,
            &last_fetched,
            &crate::profile::ProfileName::from("idle"),
            now,
            REFRESH_INTERVAL_MS
        ),
        "a fresh cache seeds even when its 5h window has reset (idle account)"
    );
    assert!(
        try_seed_cache(
            &store,
            &status,
            &last_fetched,
            &crate::profile::ProfileName::from("stale"),
            now,
            REFRESH_INTERVAL_MS
        ),
        "a cache older than one interval is still seeded as a Cached starting point"
    );
    assert!(
        !try_seed_cache(
            &store,
            &status,
            &last_fetched,
            &crate::profile::ProfileName::from("missing"),
            now,
            REFRESH_INTERVAL_MS
        ),
        "a missing cache is left for the background fetch"
    );

    assert!(store.lock().unwrap().contains_key("idle"));
    assert!(store.lock().unwrap().contains_key("stale"));
    assert!(!store.lock().unwrap().contains_key("missing"));
    assert_eq!(
        status.lock().unwrap().get("idle").copied(),
        Some(FetchStatus::Fresh),
        "a cache younger than one interval is Fresh",
    );
    assert_eq!(
        status.lock().unwrap().get("stale").copied(),
        Some(FetchStatus::Cached),
        "a cache older than one interval is Cached",
    );

    // Stamped at the ~30s-old cache mtime, not `now` — so `partition_due` resumes
    // the cadence (next ≈ mtime + interval, ~30s short of full) instead of
    // resetting the countdown.
    let stamp = last_fetched
        .lock()
        .unwrap()
        .get("idle")
        .copied()
        .unwrap()
        .as_millis();
    assert!(
        stamp <= now.saturating_sub(20_000) && stamp >= now.saturating_sub(40_000),
        "stamped at the ~30s-old cache mtime (resume), not now"
    );
}

/// `deadline_spread` separates profiles' fetch deadlines so they don't fall due
/// on the same tick: bounded to `[0, interval/4)`, deterministic for a fixed
/// `(name, now)`, varied across profiles and across cycles, and zero on a
/// degenerate interval (no modulo-by-zero).
#[test]
fn deadline_spread_is_bounded_per_profile_and_per_cycle() {
    use super::deadline_spread;

    let interval = REFRESH_INTERVAL_MS;
    let span = interval / 4;
    let now = EpochMs::from_millis(1_700_000_000_000);
    let sp = |name: &str, t: EpochMs| {
        deadline_spread(&crate::profile::ProfileName::from(name), t, interval).0
    };

    // Bounded and deterministic.
    assert!(sp("alpha", now) < span, "spread stays under interval/4");
    assert_eq!(
        sp("alpha", now),
        sp("alpha", now),
        "deterministic per (name, now)"
    );

    // Varies across profiles (8 distinct names can't all collide).
    let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let by_name: Vec<u64> = names.iter().map(|n| sp(n, now)).collect();
    assert!(
        by_name.iter().any(|&s| s != by_name[0]),
        "distinct profiles get distinct phase offsets"
    );

    // Re-rolls per cycle (different `now` for the same name).
    let by_cycle: Vec<u64> = (0..8)
        .map(|i| sp("alpha", EpochMs::from_millis(1_700_000_000_000 + i * 7_000)))
        .collect();
    assert!(
        by_cycle.iter().any(|&s| s != by_cycle[0]),
        "the jitter re-rolls as the cycle advances"
    );

    // Degenerate interval → no spread.
    assert_eq!(
        deadline_spread(&crate::profile::ProfileName::from("alpha"), now, 0).0,
        0
    );
}

/// `filter_suppressed` drops third-party entries suppressed under the SAME
/// credential they still carry, and passes the rest through in order; an empty
/// map (the steady state for healthy profiles) is a no-op fast path.
#[test]
fn filter_suppressed_drops_only_named_entries() {
    let suppressed: SuppressedGenericStore = Arc::new(RankedMutex::new(HashMap::new()));
    let victim = tp_entry("no-data");
    suppressed
        .lock()
        .unwrap()
        .insert("no-data".to_string(), victim.credential_fingerprint());

    let snap = vec![tp_entry("ok"), tp_entry("no-data"), tp_entry("also-ok")];
    let out = filter_suppressed(&suppressed, snap);
    let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["ok", "also-ok"]);

    // Empty map → identity (the fast path).
    let empty: SuppressedGenericStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snap2 = vec![tp_entry("ok"), tp_entry("no-data")];
    assert_eq!(filter_suppressed(&empty, snap2).len(), 2);
}

/// A re-login is the ONLY thing that can clear an `AuthExpired`, and a headless
/// daemon has no `refetch_queue` writer — nothing in `src/daemon/` ever inserts
/// one, so a name-keyed suppression outlived every re-login until the process
/// restarted. The daemon DOES rebuild these entries from the reloaded config
/// (`daemon::tick::rebuild_tokens`), so the changed credential is the signal.
#[test]
fn filter_suppressed_re_admits_an_entry_whose_credential_changed() {
    let suppressed: SuppressedGenericStore = Arc::new(RankedMutex::new(HashMap::new()));
    let expired = alibaba_entry("qwen", "dead-console-token");
    suppressed
        .lock()
        .unwrap()
        .insert("qwen".to_string(), expired.credential_fingerprint());

    // Same credential → still suppressed, no cadence retry.
    assert!(filter_suppressed(&suppressed, vec![expired]).is_empty());

    // Re-login wrote a new console session; the rebuilt entry carries it.
    let relogged = alibaba_entry("qwen", "fresh-console-token");
    let out = filter_suppressed(&suppressed, vec![relogged]);
    assert_eq!(
        out.len(),
        1,
        "a re-login must re-admit the profile without a restart or a timer",
    );
}

/// The api key is part of the fingerprint too, so the generic no-data
/// suppression clears on a rotated key by the same mechanism.
#[test]
fn filter_suppressed_re_admits_a_generic_entry_on_a_rotated_key() {
    let suppressed: SuppressedGenericStore = Arc::new(RankedMutex::new(HashMap::new()));
    let old = tp_entry("proxy");
    suppressed
        .lock()
        .unwrap()
        .insert("proxy".to_string(), old.credential_fingerprint());
    let mut rotated = tp_entry("proxy");
    rotated.api_key = "a-different-key".to_string();
    assert_eq!(filter_suppressed(&suppressed, vec![rotated]).len(), 1);
}

fn tp_entry(name: &str) -> ThirdPartyEntry {
    ThirdPartyEntry {
        name: crate::profile::ProfileName::from(name),
        target: crate::providers::ThirdPartyTarget::Generic {
            base_url: "https://example.com".to_string(),
        },
        api_key: "key".to_string(),
    }
}

fn alibaba_entry(name: &str, token: &str) -> ThirdPartyEntry {
    ThirdPartyEntry {
        name: crate::profile::ProfileName::from(name),
        target: crate::providers::ThirdPartyTarget::Known {
            provider: crate::providers::Provider::Alibaba,
            console: Some(crate::profile::ConsoleCredential {
                token: token.to_string(),
                site: crate::profile::ConsoleSite::International,
                region: "ap-southeast-1".to_string(),
            }),
        },
        api_key: String::new(),
    }
}

/// Alibaba's quota surface cannot read the api key at all, so a console-only
/// profile is still fetchable. Dropped from the work list it would never get a
/// `fetch_status`, and the Usage tab would spin "loading" forever.
#[test]
fn collect_third_party_entries_keeps_a_keyless_alibaba_profile() {
    let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("qwen"));
    p.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    p.provider =
        crate::providers::Provider::from_base_url(p.base_url.as_deref().unwrap_or_default());
    p.api_key = None;

    let entries = collect_third_party_entries(std::slice::from_ref(&p));
    assert_eq!(
        entries.len(),
        1,
        "a console-only Alibaba profile still belongs on the third-party leg",
    );
    assert!(crate::usage::third_party_credentialed(&p));

    // A keyless DeepSeek profile has no credential at all and stays out — the
    // render layer says so instead of loading forever.
    let mut ds = crate::testutil::blank_profile(&crate::profile::ProfileName::from("ds"));
    ds.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    ds.provider = crate::providers::Provider::from_base_url("https://api.deepseek.com/anthropic");
    assert!(!crate::usage::third_party_credentialed(&ds));
    assert!(collect_third_party_entries(std::slice::from_ref(&ds)).is_empty());
}

/// Third-party startup seed mirrors the OAuth one: any cached profile is seeded
/// with `last_fetched` stamped at the cache mtime (cadence resumes) — `Fresh` when
/// younger than one interval, `Cached` when older (refreshed in the background). A
/// missing cache is left for the scheduler.
#[test]
fn bootstrap_third_party_seeds_any_cache() {
    use std::time::{Duration, SystemTime};

    use super::{
        FetchStatus, ThirdPartyStatusStore, ThirdPartyUsageStore, bootstrap_third_party, now_ms,
    };
    use crate::profile::profile_subpath;
    use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, write_profile_cache};
    use crate::providers::{ThirdPartyStats, UsageBar};
    use crate::testutil::{HomeSandbox, set_mtime};

    let _home = HomeSandbox::new();
    let store: ThirdPartyUsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));

    let stats = |pct: f64| ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![UsageBar {
            label: "5h".to_string(),
            pct,
            resets_at: None,
            used: None,
            total: None,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    // Fresh cache (just written) seeds `Fresh`; a 2h-old cache seeds `Cached`.
    crate::testutil::register_names(&["cached", "stale"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("cached"),
        THIRD_PARTY_CACHE_FILE,
        &stats(12.0),
    );
    write_profile_cache(
        &crate::profile::ProfileName::from("stale"),
        THIRD_PARTY_CACHE_FILE,
        &stats(20.0),
    );
    let stale_path = profile_subpath(
        &crate::profile::ProfileName::from("stale"),
        "third_party_cache.json",
    )
    .expect("stale path");
    set_mtime(
        &stale_path,
        SystemTime::now() - Duration::from_secs(2 * 3600),
    );

    let entries = vec![tp_entry("cached"), tp_entry("stale"), tp_entry("missing")];
    bootstrap_third_party(
        &store,
        &status,
        &last_fetched,
        &entries,
        REFRESH_INTERVAL_MS,
    );

    assert!(
        store.lock().unwrap().contains_key("cached"),
        "a fresh third-party cache is seeded from disk"
    );
    assert!(
        store.lock().unwrap().contains_key("stale"),
        "a stale third-party cache is still seeded as a Cached starting point"
    );
    assert!(
        !store.lock().unwrap().contains_key("missing"),
        "a profile with no cache is left for the scheduler"
    );
    assert_eq!(
        status.lock().unwrap().get("cached").copied(),
        Some(FetchStatus::Fresh),
        "a third-party cache younger than one interval surfaces as Fresh"
    );
    assert_eq!(
        status.lock().unwrap().get("stale").copied(),
        Some(FetchStatus::Cached),
        "a third-party cache older than one interval surfaces as Cached"
    );
    assert!(
        !last_fetched.lock().unwrap().contains_key("missing"),
        "a no-cache profile is left unstamped so it fetches on the first tick"
    );
    // Stamped at the cache mtime (~now, just written), so the cadence resumes.
    let now = now_ms();
    let stamp = last_fetched
        .lock()
        .unwrap()
        .get("cached")
        .copied()
        .unwrap()
        .as_millis();
    assert!(
        stamp <= now && stamp >= now.saturating_sub(5_000),
        "the seeded third-party profile stamps last_fetched at the cache mtime"
    );
}

// ── AUTH-1: proactive auth-health during the usage poll ──────────────────────
// `refresh_failure_is_terminal` decides whether a poll-time refresh failure means
// the OAuth login DROPPED (quarantine the account now) or is a transient blip
// (leave the flag, retry). This is the classification behind the account surfacing
// "needs reauth" on the tick the drop is detected, not only on the next switch.

#[test]
fn dead_refresh_token_is_terminal() {
    // A 4xx from the token endpoint (revoked / expired refresh token) → the login
    // is gone; quarantine so the UI surfaces reauth immediately.
    let err = RefreshError::Invalid(crate::oauth::TokenFailure::Status(400));
    assert!(super::refresh_failure_is_terminal(&err));
}

#[test]
fn transient_refresh_failure_is_not_terminal() {
    // A network / 5xx / parse blip must NOT quarantine — the token may be fine; the
    // fixed cadence retries next tick.
    let err = RefreshError::Transient(crate::oauth::TokenFailure::Transport);
    assert!(!super::refresh_failure_is_terminal(&err));
}

// `fresher_disk_pair` is the double-spend guard in front of the quarantine: a
// terminal 400 is also what a benign single-use double-spend returns (Claude
// Code refreshing the active profile's symlinked credentials mid-poll, or a
// refresher that completed before this tick's guard was acquired). Only an
// UNCHANGED on-disk pair proves a real revocation.

#[test]
fn a_disk_pair_that_moved_past_the_spent_token_is_returned_not_quarantined() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = "double-spend-benign";
    let mut p = crate::profile::Profile::new(name.to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-new".into(),
            refresh_token: Some("rt-new".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");

    // We spent "rt-old"; the store moved to "rt-new" — someone else rotated.
    assert_eq!(
        super::fresher_disk_pair(&crate::profile::ProfileName::from(name), "rt-old"),
        Some(("at-new".to_string(), Some("rt-new".to_string())))
    );
    // We spent "rt-new" itself and it 400d — a real revocation, quarantine.
    assert_eq!(
        super::fresher_disk_pair(&crate::profile::ProfileName::from(name), "rt-new"),
        None
    );
}

/// The carry path must also LIFT a stale quarantine: the moved pair proves the
/// chain is alive, and without the clear, an account recovered by an external
/// re-login stays excluded from the fallback walk and refused by every switch
/// gate forever (its own refresh never succeeds — the carry preempts it).
#[test]
fn carrying_an_external_rotation_clears_a_stale_quarantine() {
    use crate::lockorder::RankedMutex;
    use std::sync::Arc;

    let _home = crate::testutil::HomeSandbox::new();
    let name = "double-spend-quarantined";
    let mut p = crate::profile::Profile::new(name.to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-new".into(),
            refresh_token: Some("rt-new".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![p],
    };
    config.state.profiles = vec![name.into()];
    config.set_auth_broken(&crate::profile::ProfileName::from(name), true);
    let handle: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(config));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(Default::default()));

    // Spent "rt-old"; store holds "rt-new" → carry fires and lifts the flag.
    let outcome = super::carry_external_rotation(
        &handle,
        &crate::profile::ProfileName::from(name),
        "rt-old",
        &refetch,
    );
    assert!(outcome.is_some(), "a moved pair must carry");
    assert!(
        !handle
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from(name)),
        "the carried (alive) chain must lift a stale quarantine"
    );
    assert!(
        refetch.lock().unwrap().contains(name),
        "the carried pair is refetched next tick"
    );

    // Spent the store's own pair → no carry, and the flag is left alone.
    handle
        .lock()
        .unwrap()
        .set_auth_broken(&crate::profile::ProfileName::from(name), true);
    let outcome = super::carry_external_rotation(
        &handle,
        &crate::profile::ProfileName::from(name),
        "rt-new",
        &refetch,
    );
    assert!(outcome.is_none(), "an unchanged pair is a real revocation");
    assert!(
        handle
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from(name)),
        "a real revocation keeps the quarantine"
    );
}

#[test]
fn a_missing_or_tokenless_profile_never_reads_as_a_benign_double_spend() {
    let _home = crate::testutil::HomeSandbox::new();
    // No profile on disk at all.
    assert_eq!(
        super::fresher_disk_pair(
            &crate::profile::ProfileName::from("double-spend-missing"),
            "rt-x"
        ),
        None
    );
    // Profile exists but has no stored credentials.
    let p = crate::profile::Profile::new("double-spend-bare".to_string(), None, None);
    crate::profile::save_profile(&p).expect("save profile");
    assert_eq!(
        super::fresher_disk_pair(
            &crate::profile::ProfileName::from("double-spend-bare"),
            "rt-x"
        ),
        None
    );
}

// `token_clock_expired` gates whether a 429 on the usage fetch falls through to the
// refresh leg (the AUTH-1 fix so a dead login that 429s surfaces as auth_broken
// instead of being masked as RateLimited forever) vs bails to cache. Only a
// clock-EXPIRED token is worth spending the single-use refresh on.

#[test]
fn rate_limited_expired_token_rotates_so_a_dead_login_surfaces() {
    // 429 + access token expired 1s ago → rotate (a dead refresh token then flags
    // auth_broken; a live one just re-fetches). now=10_000ms, exp=9_000ms.
    assert!(super::token_clock_expired(Some(9_000), 10_000));
}

#[test]
fn rate_limited_valid_token_does_not_rotate() {
    // 429 on a still-valid token is a pure endpoint rate limit — refusing to refresh
    // protects the single-use token from being re-spent every tick. exp in the future.
    assert!(!super::token_clock_expired(Some(20_000), 10_000));
}

#[test]
fn rate_limited_unknown_expiry_does_not_rotate() {
    // No expiry known → conservative: never spend a refresh on a token we can't prove
    // is expired (matches auto_start_kick's `is_some_and` gate).
    assert!(!super::token_clock_expired(None, 10_000));
}

// `classify_pre_rotation` is the pure classifier `fetch_with_rotation` extracts
// its branch selection into — no I/O, no clock read, so the truth table below
// exercises it without live HTTP. `token_clock_expired` is passed in as an
// already-computed bool (never re-derived inside the classifier).

#[test]
fn pre_rotation_serves_a_live_body() {
    use super::{PreRotationDecision, classify_pre_rotation};
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let info = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Pro,
            subscription_status: None,
        }),
        ..UsageInfo::default()
    };
    match classify_pre_rotation(Ok(info), false) {
        PreRotationDecision::Serve(served) => {
            assert_eq!(served.plan.expect("plan").tier, PlanTier::Pro);
        }
        other => panic!("expected Serve, got {other:?}"),
    }
}

#[test]
fn pre_rotation_429_on_a_valid_token_bails_rate_limited_with_plan() {
    use std::time::Duration;

    use super::{FetchError, PreRotationDecision, classify_pre_rotation};
    use crate::usage::{PlanInfo, PlanTier};

    let err = FetchError::RateLimited {
        retry_after: Some(Duration::from_secs(30)),
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
    };
    // token_clock_expired == false: a still-valid token's 429 is a pure
    // endpoint rate limit — bail to cache, plan and retry_after both intact.
    match classify_pre_rotation(Err(err), false) {
        PreRotationDecision::BailRateLimited { retry_after, plan } => {
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
            let plan = plan.expect("plan rides along on a live-token 429");
            assert_eq!(plan.tier, PlanTier::Free);
            assert_eq!(plan.subscription_status.as_deref(), Some("canceled"));
        }
        other => panic!("expected BailRateLimited, got {other:?}"),
    }
}

#[test]
fn pre_rotation_401_rotates_without_an_unmask_hint() {
    use super::{FetchError, PreRotationDecision, classify_pre_rotation};

    match classify_pre_rotation(Err(FetchError::Status(401)), false) {
        PreRotationDecision::Rotate { unmask_429 } => assert_eq!(unmask_429, None),
        other => panic!("expected Rotate, got {other:?}"),
    }
}

#[test]
fn pre_rotation_429_on_an_expired_token_rotates_and_drops_the_plan() {
    use std::time::Duration;

    use super::{FetchError, PreRotationDecision, classify_pre_rotation};
    use crate::usage::{PlanInfo, PlanTier};

    let with_hint = FetchError::RateLimited {
        retry_after: Some(Duration::from_secs(5)),
        plan: Some(PlanInfo {
            tier: PlanTier::Pro,
            subscription_status: None,
        }),
    };
    // token_clock_expired == true: falls through to rotation. The `Rotate`
    // variant has no plan field at all — the dead-token plan is dropped by
    // construction, not just by convention.
    match classify_pre_rotation(Err(with_hint), true) {
        PreRotationDecision::Rotate { unmask_429 } => {
            assert_eq!(unmask_429, Some(Some(Duration::from_secs(5))));
        }
        other => panic!("expected Rotate, got {other:?}"),
    }

    let no_hint = FetchError::RateLimited {
        retry_after: None,
        plan: None,
    };
    match classify_pre_rotation(Err(no_hint), true) {
        PreRotationDecision::Rotate { unmask_429 } => assert_eq!(unmask_429, Some(None)),
        other => panic!("expected Rotate, got {other:?}"),
    }
}

#[test]
fn pre_rotation_other_errors_bail_to_cache() {
    use super::{FetchError, PreRotationDecision, classify_pre_rotation};

    assert!(matches!(
        classify_pre_rotation(Err(FetchError::Network), false),
        PreRotationDecision::BailCached
    ));
    assert!(matches!(
        classify_pre_rotation(Err(FetchError::Parse), false),
        PreRotationDecision::BailCached
    ));
}

// `proactive_rotation_due` decides whether a profile rotates AHEAD of expiry
// instead of waiting for a 401. Three inputs: the `preemptive_rotation`
// toggle (default ON), the CLA-ROLL flag (which ORs over the toggle, never
// over the clock), and whether the stored expiry sits inside the lead
// window. Liveness, active-ness and the Keychain are NOT inputs — every
// non-isolated session reads the same credential file clauth rotates.

#[test]
fn preemptive_rotation_is_on_by_default_and_the_toggle_still_disables_it() {
    assert!(crate::profile::AppState::default().preemptive_rotation);
    // Toggled off, a token deep inside the lead window stays lazy.
    assert!(!super::proactive_rotation_due(
        false,
        false,
        Some(10_000),
        10_000,
        90_000
    ));
}

#[test]
fn proactive_rotation_fires_only_inside_the_lead_window() {
    let interval = 90_000u64;
    let lead = super::rotate_lead_ms(interval);
    // At or inside the lead window → rotate now, so the token never reaches
    // the 5-minute mark where the running claude would refresh it itself.
    assert!(super::proactive_rotation_due(
        true,
        false,
        Some(10_000 + lead),
        10_000,
        interval
    ));
    assert!(super::proactive_rotation_due(
        true,
        false,
        Some(10_000),
        10_000,
        interval
    ));
    // Beyond the lead window → plain poll; nothing at stake yet.
    assert!(!super::proactive_rotation_due(
        true,
        false,
        Some(10_000 + lead + 1),
        10_000,
        interval
    ));
}

#[test]
fn proactive_lead_scales_with_the_poll_interval_with_a_floor() {
    // The lead is derived from the cadence (3 polls' worth of rotation
    // opportunities before expiry) and never drops below the floor.
    assert_eq!(super::ROTATE_LEAD_FLOOR_MS, 900_000);
    assert_eq!(super::rotate_lead_ms(400_000), 1_200_000);
    assert_eq!(super::rotate_lead_ms(10_000), super::ROTATE_LEAD_FLOOR_MS);
}

/// The whole point of the floor: Claude Code refreshes its own OAuth token
/// once it is within 5 MINUTES of expiry (measured against CC's shipped
/// bundle). At the SHIPPED 90 s cadence the
/// `3 × interval` term is only 4.5 min, which loses that race every time —
/// the floor is what carries it. Reds if anyone drops the floor back under
/// `300_000` or lowers it below the shipped interval's own term.
#[test]
fn the_rotation_lead_clears_claude_codes_own_five_minute_refresh_threshold() {
    const CC_REFRESH_THRESHOLD_MS: i64 = 300_000;
    let shipped = crate::profile::DEFAULT_REFRESH_INTERVAL_MS;
    assert_eq!(shipped, 90_000, "the cadence this margin is sized against");
    assert!(
        super::rotate_lead_ms(shipped) > CC_REFRESH_THRESHOLD_MS,
        "clauth must rotate before CC's own threshold, got {} ms vs {CC_REFRESH_THRESHOLD_MS} ms",
        super::rotate_lead_ms(shipped)
    );
    // The floor, not the cadence term, is what clears it at the shipped rate.
    assert!(
        (shipped as i64).saturating_mul(3) < CC_REFRESH_THRESHOLD_MS,
        "if the cadence term alone cleared 5 min the floor would be untested"
    );
}

#[test]
fn proactive_rotation_never_fires_on_unknown_expiry() {
    // Never spend a single-use refresh on a token whose expiry we can't prove.
    assert!(!super::proactive_rotation_due(
        true, false, None, 10_000, 90_000
    ));
}

/// Liveness is not an input. Under the old gate a running `clauth start`
/// session froze rotation on that account; it shares the credential file, so
/// the predicate must not care either way — there is no session parameter left
/// to pass, and this pins that the same inputs still decide.
#[test]
fn proactive_rotation_decides_on_the_toggle_and_the_clock_alone() {
    let interval = 90_000u64;
    let lead = super::rotate_lead_ms(interval);
    for now in [0i64, 10_000, 1_700_000_000_000] {
        assert!(super::proactive_rotation_due(
            true,
            false,
            Some(now + lead),
            now,
            interval
        ));
        assert!(!super::proactive_rotation_due(
            false,
            false,
            Some(now + lead),
            now,
            interval
        ));
    }
}

// ── stand-down hydrate (a live daemon owns the loop) ─────────────────────────
//
// While `standdown_tick` runs, this side never fetches or rotates — it only
// re-seeds the stores from the disk caches the daemon keeps fresh. These pin
// the hydrate contract: cache → store with a freshness-derived status and
// `last_fetched` stamped AT the cache mtime (so the published countdowns track
// the daemon's real cadence, not this process's clock).

#[test]
fn standdown_hydrate_seeds_the_store_from_the_daemon_cache() {
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::{UsageInfo, UsageWindow};
    let _home = crate::testutil::HomeSandbox::new();

    let info = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    crate::testutil::register_names(&["kitty"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        USAGE_CACHE_FILE,
        &info,
    );

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: super::StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_store: super::ThirdPartyUsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_status: super::ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));

    super::hydrate_from_daemon_caches(
        &store,
        &status,
        &tp_store,
        &tp_status,
        &last_fetched,
        &["kitty".to_string(), "cacheless".to_string()],
        &[],
        REFRESH_INTERVAL_MS,
    );

    let seeded = store.lock().unwrap().get("kitty").cloned();
    assert_eq!(
        seeded.and_then(|i| i.five_hour.map(|w| w.utilization)),
        Some(42.0),
        "the daemon-written cache lands in the live store"
    );
    // A just-written cache (mtime ≈ now) is inside the fetch window → Fresh,
    // and its stamp anchors the countdown to the daemon's write time.
    assert_eq!(
        status.lock().unwrap().get("kitty").copied(),
        Some(super::FetchStatus::Fresh),
    );
    let stamp = last_fetched.lock().unwrap().get("kitty").copied();
    let now = super::now_ms();
    assert!(
        stamp.is_some_and(|s| now.saturating_sub(s.as_millis()) < 30_000),
        "last_fetched stamped at the cache mtime: {stamp:?} vs now {now}"
    );

    // No cache → left untouched (the daemon publishes it shortly); never a
    // synthetic entry that would render as data.
    assert!(store.lock().unwrap().get("cacheless").is_none());
    assert!(status.lock().unwrap().get("cacheless").is_none());
    assert!(last_fetched.lock().unwrap().get("cacheless").is_none());
}

/// Re-hydrating every tick must track the daemon's writes: a NEWER cache body
/// replaces the seeded one (same profile, later mtime), never the reverse.
#[test]
fn standdown_hydrate_follows_the_daemon_cache_forward() {
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::{UsageInfo, UsageWindow};
    let _home = crate::testutil::HomeSandbox::new();

    let at = |util: f64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: super::StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_store: super::ThirdPartyUsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_status: super::ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let hydrate = |seed_names: &[String]| {
        super::hydrate_from_daemon_caches(
            &store,
            &status,
            &tp_store,
            &tp_status,
            &last_fetched,
            seed_names,
            &[],
            REFRESH_INTERVAL_MS,
        )
    };

    crate::testutil::register_names(&["kitty"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        USAGE_CACHE_FILE,
        &at(10.0),
    );
    hydrate(&["kitty".to_string()]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        USAGE_CACHE_FILE,
        &at(55.0),
    );
    hydrate(&["kitty".to_string()]);

    let seeded = store.lock().unwrap().get("kitty").cloned();
    assert_eq!(
        seeded.and_then(|i| i.five_hour.map(|w| w.utilization)),
        Some(55.0),
        "the daemon's newer write wins on the next hydrate"
    );
}

/// `standdown_tick` end to end (minus the probe): forced names from a manual
/// `r` are drained and their Queued marks cleared (the daemon can't be asked
/// to fetch early — a stranded mark freezes the row spinner), the store is
/// hydrated, and countdowns are published off the cache stamp. Nothing here
/// performs HTTP: every assertion is served by disk state alone.
#[test]
fn standdown_tick_drains_forced_and_publishes_countdowns() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::UsageInfo;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let _home = crate::testutil::HomeSandbox::new();

    crate::testutil::register_names(&["kitty"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        USAGE_CACHE_FILE,
        &UsageInfo::default(),
    );

    // The standby seed sources names from config (the display superset), so the
    // profile whose cache is hydrated must live there — as it does in production.
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile_disabled("kitty", false)],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![token("kitty")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(true),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    // A manual `r` landed just before this tick: forced name + Queued mark.
    state.refetch_queue.lock().unwrap().insert("kitty".into());
    mark_activity(
        &state.activity,
        &crate::profile::ProfileName::from("kitty"),
        ProfileActivity::Queued,
    );

    super::standdown_tick(&state, REFRESH_INTERVAL_MS);

    assert!(
        state.refetch_queue.lock().unwrap().is_empty(),
        "forced names are consumed, not left to pile up"
    );
    assert!(
        state.activity.lock().unwrap().get("kitty").is_none(),
        "the Queued mark is cleared — no frozen spinner"
    );
    assert!(
        state.store.lock().unwrap().contains_key("kitty"),
        "the store is hydrated from the daemon cache"
    );
    let next = state
        .next_refresh_per_profile
        .lock()
        .unwrap()
        .get("kitty")
        .copied();
    let stamp = state
        .last_fetched
        .lock()
        .unwrap()
        .get("kitty")
        .map(|e| e.as_millis());
    assert_eq!(
        next,
        stamp.map(|s| s + REFRESH_INTERVAL_MS),
        "the countdown tracks the cache stamp + one interval"
    );
}

/// The bootstrap pre-marks cache-due profiles `Queued` for first paint,
/// expecting a fetch worker to take over — standing down, no worker exists, so
/// the tick must sweep EVERY Queued mark (not only forced ones) or the row
/// spins forever where the daemon-fed countdown belongs. In-flight kinds stay.
#[test]
fn standdown_sweeps_bootstrap_queued_marks() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::UsageInfo;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let _home = crate::testutil::HomeSandbox::new();

    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        USAGE_CACHE_FILE,
        &UsageInfo::default(),
    );

    // The standby seed sources names from config (the display superset), so the
    // profile whose cache is hydrated must live there — as it does in production.
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile_disabled("kitty", false)],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![token("kitty"), token("stale")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(true),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    // Bootstrap pre-marked a cache-due profile; a rotate worker from the last
    // armed tick is still in flight on another.
    mark_activity(
        &state.activity,
        &crate::profile::ProfileName::from("stale"),
        ProfileActivity::Queued,
    );
    mark_activity(
        &state.activity,
        &crate::profile::ProfileName::from("kitty"),
        ProfileActivity::Refreshing,
    );

    super::standdown_tick(&state, REFRESH_INTERVAL_MS);

    let a = state.activity.lock().unwrap();
    assert!(
        a.get("stale").is_none(),
        "an un-owned Queued mark is swept — no frozen spinner"
    );
    assert!(
        matches!(a.get("kitty"), Some(ProfileActivity::Refreshing)),
        "an in-flight worker's mark survives (it clears itself on landing)"
    );
}

/// Single-fetcher lease (#27): when another instance already holds
/// `usage-fetch.lock`, `tick` must take the stand-down path. Driven through the
/// real `tick` (not `standdown_tick`) so the lease branch itself is pinned: an
/// external holder forces `fetch_lease.acquire()` to return `false`.
///
/// `kitty` is stamped NOT due on purpose, which is what makes every assertion
/// below discriminate between the two branches — and keeps a regression cheap:
///   * armed + nothing due never calls `fetch_oauth_due`, so a broken lease
///     fails these asserts instead of firing a live request at the real endpoint
///     (`tick` hardcodes the real fetcher; there is no seam to inject through it);
///   * the `Queued` mark survives an armed tick (`clear_orphaned_forced` returns
///     early on an empty `forced` set, and no worker runs to clear it), while
///     `standdown_tick` sweeps EVERY `Queued` mark — so the sweep proves the
///     branch. Were `kitty` due, the armed path would mark-then-clear it too and
///     the assert would pass either way.
///   * the store is only seeded by the stand-down hydrate here; an armed tick
///     with nothing due never reaches `apply_outcome`, so it stays empty.
#[test]
fn tick_stands_down_when_another_instance_holds_the_fetch_lease() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::UsageInfo;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let _home = crate::testutil::HomeSandbox::new();

    // Another instance wins the lease first; its `File` must stay alive so the
    // flock stays held for the rest of the test.
    let other = crate::daemon::FetchLease::new();
    assert!(other.acquire(), "the first instance wins the lease");

    crate::testutil::register_names(&["kitty"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        USAGE_CACHE_FILE,
        &UsageInfo::default(),
    );
    // The standby seed sources names from config (the display superset), so the
    // profile whose cache is hydrated must live there — as it does in production.
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile_disabled("kitty", false)],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![token("kitty")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        // A DIFFERENT lease over the same file → its acquire() is denied while
        // `other` holds the flock.
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    // Stamp `kitty` as just-fetched so it is NOT due this tick: an armed tick
    // would then fetch nothing (no live request on a regression) and leave the
    // marks/store below untouched, which is what makes each assert discriminate.
    state
        .last_fetched
        .lock()
        .unwrap()
        .insert("kitty".to_string(), EpochMs::from_millis(super::now_ms()));

    // A bootstrap-only `Queued` mark: `standdown_tick` sweeps every Queued mark,
    // while an armed tick with nothing due leaves it in place.
    mark_activity(
        &state.activity,
        &crate::profile::ProfileName::from("kitty"),
        ProfileActivity::Queued,
    );

    super::tick(&state);

    assert!(
        state.activity.lock().unwrap().get("kitty").is_none(),
        "stood down: the Queued mark is swept, never driven into a fetch"
    );
    assert!(
        state.store.lock().unwrap().contains_key("kitty"),
        "stood down: the store is hydrated from the shared cache"
    );
    assert!(
        state.standdown_active.load(Ordering::Relaxed),
        "the stand-down edge is recorded"
    );
    drop(other);
}

/// `tick`'s third-party leg, end to end against a real loopback provider. The
/// only other `tick` test plants a competing lease and takes the stand-down
/// return, so everything past that check — the snapshot, the partition, the
/// spawned leg, and the store/status/stamp writes it lands — was dark.
///
/// No seam is needed: a `Generic` target's `base_url` IS one. `api_origin` takes
/// the listener's `http://127.0.0.1:PORT` verbatim and the generic engine probes
/// its curated paths against exactly that origin. An empty `tokens` list keeps
/// the OAuth leg out of the tick, and `EndpointSandbox` also points the OAuth
/// endpoints at the SAME listener, so a future leak would be RECORDED here
/// rather than silently escaping to the real Anthropic host.
#[test]
fn tick_fetches_the_third_party_leg_under_its_own_lease() {
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let home = crate::testutil::HomeSandbox::new();

    let name = "gen";
    // One request is a correct run (the first candidate path answers); `max` sits
    // above it so a regression that walks the whole candidate list is recorded
    // rather than silently refused a socket.
    let (base, server) = crate::testutil::serve_endpoints(4, |_, _| {
        (200, r#"{"session":{"percent":42.5}}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    profile.base_url = Some(base.clone());
    profile.api_key = Some("key".to_string());
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    }));
    let entry = ThirdPartyEntry {
        name: crate::profile::ProfileName::from(name),
        target: crate::providers::ThirdPartyTarget::Generic {
            base_url: base.clone(),
        },
        api_key: "key".to_string(),
    };
    let state = super::SchedulerState {
        config,
        // Empty on purpose: an OAuth work-list would fire at the real endpoint.
        tokens: Arc::new(RankedMutex::new(vec![])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![entry])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        // Nothing else holds the flock, so this tick is the fetcher.
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    super::tick(&state);
    let seen = server.join().expect("listener");

    // Leak assert BEFORE the count: a leak trips the count too, and the count's
    // message names the wrong defect. Ordered this way the failure text matches
    // the failure.
    assert!(
        !seen.iter().any(|p| {
            p.starts_with("/v1/oauth/token")
                || p.starts_with("/api/oauth/usage")
                || p.starts_with("/api/oauth/profile")
                || p.starts_with("/v1/messages")
        }),
        "an empty `tokens` work-list must never reach the OAuth endpoints: {seen:?}"
    );
    assert_eq!(
        seen.len(),
        1,
        "the answering candidate ends the probe: {seen:?}"
    );
    let bars = state
        .third_party_usage_store
        .lock()
        .unwrap()
        .get(name)
        .map(|s| s.bars.clone())
        .expect("the leg must land its stats in the store");
    assert_eq!(bars.len(), 1);
    assert!((bars[0].pct - 42.5).abs() < f64::EPSILON, "got {bars:?}");
    assert_eq!(
        state.third_party_status.lock().unwrap().get(name).copied(),
        Some(super::FetchStatus::Fresh),
        "a landed body is Fresh, not a cache fallback"
    );
    assert!(
        state.last_fetched.lock().unwrap().contains_key(name),
        "the fetch stamps its slot, or the profile re-fetches every tick"
    );
    assert!(
        state.activity.lock().unwrap().get(name).is_none(),
        "the worker clears its own spinner on landing"
    );
}

/// Pins two tick legs the existing armed-tick test does not cover: the
/// history-prune leg and the cadence-window throttle.
///
///   * **prune** — `last_history_prune` is seeded stale, so the tick's
///     `prune_histories_if_due` fires and advances the stamp; deleting the
///     call from `tick` leaves the stamp stale and reds this test.
///   * **throttle** — a second tick immediately after the first must NOT
///     re-fetch, because `partition_due` sees `last_fetched + interval` is
///     still in the future; removing that gate makes the second tick fire a
///     second HTTP request and `seen.len()` reads 2.
///
/// The OAuth/session-token leg is NOT pinned here: `tick` hardcodes the real
/// fetcher against a constant `api.anthropic.com` URL with no injection seam,
/// so an OAuth work-list would fire a live request. The `tokens` list is empty
/// for the same reason as the test above.
#[test]
fn tick_prunes_histories_and_throttles_a_second_tick_inside_the_cadence_window() {
    use super::HISTORY_PRUNE_INTERVAL_MS;
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let home = crate::testutil::HomeSandbox::new();

    let name = "gen";
    // Allow 2 requests so a broken throttle RECORDS the second hit rather than
    // timing out; assert below that only 1 arrives.
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (200, r#"{"session":{"percent":42.5}}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    profile.base_url = Some(base.clone());
    profile.api_key = Some("key".to_string());
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    }));
    let entry = ThirdPartyEntry {
        name: crate::profile::ProfileName::from(name),
        target: crate::providers::ThirdPartyTarget::Generic {
            base_url: base.clone(),
        },
        api_key: "key".to_string(),
    };
    let stale_prune = crate::usage::now_ms().saturating_sub(HISTORY_PRUNE_INTERVAL_MS + 1);
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![entry])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(stale_prune),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    // First tick: wins the lease, prunes histories, fetches.
    super::tick(&state);

    assert!(
        state.last_history_prune.load(Ordering::Relaxed) > stale_prune,
        "the history prune leg advanced the stale stamp"
    );
    assert!(
        state.last_fetched.lock().unwrap().contains_key(name),
        "the first tick stamped last_fetched"
    );

    // Second tick inside the cadence window: partition_due sees
    // last_fetched + interval > now, so no fetch fires.
    super::tick(&state);

    let seen = server.join().expect("listener");
    assert_eq!(
        seen.len(),
        1,
        "a second tick inside the cadence window must not re-fetch: {seen:?}"
    );
}

/// Production wiring pin for the queue election. Both lapsed members are due,
/// but `tick` must elect exactly one BEFORE the OAuth fan-out; without that call
/// both permissive `TokenEntry` snapshots kick and open together.
#[test]
fn auto_start_queue_election_is_wired_into_tick() {
    use crate::profile::{AppConfig, AppState};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let home = crate::testutil::HomeSandbox::new();
    let now_before = crate::usage::now_epoch_secs();
    let usage_body = format!(
        concat!(
            r#"{{"five_hour":{{"utilization":1.0,"resets_at":"{}"}},"#,
            r#""seven_day":{{"utilization":1.0,"resets_at":"{}"}}}}"#
        ),
        epoch_secs_to_iso(now_before + 5 * 3600),
        epoch_secs_to_iso(now_before + 7 * 24 * 3600),
    );
    // A correct run makes five requests (one kick, two usage, two profile).
    // Six leaves room to record the second kick when the election wiring is
    // removed: the listener, rather than a refused socket, then catches it.
    let (base, server) = crate::testutil::serve_endpoints(6, move |path, _| {
        if path.starts_with("/v1/messages") {
            (200, "{}".to_string())
        } else if path.starts_with("/api/oauth/usage") {
            (200, usage_body.clone())
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let app_config = AppConfig {
        state: AppState {
            fallback_chain: vec!["a".into(), "b".into()],
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![auto_start_queue_profile("a"), auto_start_queue_profile("b")],
    };
    let tokens = super::collect_tokens(&app_config);
    let lapsed = || UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(now_before - 60)),
        }),
        ..UsageInfo::default()
    };
    let store = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), lapsed()),
        ("b".to_string(), lapsed()),
    ])));
    let auto_start_queue = crate::usage::new_auto_start_queue_state();
    let state = super::SchedulerState {
        config: Arc::new(RankedMutex::new(app_config)),
        tokens: Arc::new(RankedMutex::new(tokens)),
        store: store.clone(),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: auto_start_queue.clone(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    super::tick(&state);
    let seen = server.join().expect("listener");
    assert_eq!(
        seen.iter()
            .filter(|path| path.starts_with("/v1/messages"))
            .count(),
        1,
        "the tick elects one opener before the fan-out: {seen:?}"
    );
    assert!(
        crate::usage::queue_anchor_cached(&auto_start_queue).is_some(),
        "the elected kick moves the scheduler's shared queue anchor"
    );
    assert!(
        !super::window_lapsed(&store, &"a".into(), crate::usage::now_epoch_secs()),
        "the first queue member is elected and its stored window opens"
    );
}

/// The landed-kick path distinguishes a live-window health re-test from a real
/// lapsed-window open. Only the latter may move the queue anchor or emit the
/// queue-open event, and the anchor must carry the real wall clock (not epoch 0).
#[test]
fn auto_start_queue_run_fetch_anchors_and_logs_only_a_lapsed_window_open() {
    use crate::profile::{AppConfig, AppState};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};

    let home = crate::testutil::HomeSandbox::new();
    let now_before = crate::usage::now_epoch_secs();
    let usage_body = format!(
        concat!(
            r#"{{"five_hour":{{"utilization":1.0,"resets_at":"{}"}},"#,
            r#""seven_day":{{"utilization":1.0,"resets_at":"{}"}}}}"#
        ),
        epoch_secs_to_iso(now_before + 5 * 3600),
        epoch_secs_to_iso(now_before + 7 * 24 * 3600),
    );
    let (base, server) = crate::testutil::serve_endpoints(6, move |path, _| {
        if path.starts_with("/v1/messages") {
            (200, "{}".to_string())
        } else if path.starts_with("/api/oauth/usage") {
            (200, usage_body.clone())
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let app_config = AppConfig {
        state: AppState {
            fallback_chain: vec!["a".into()],
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![auto_start_queue_profile("a")],
    };
    let mut entry = super::collect_tokens(&app_config)
        .into_iter()
        .next()
        .expect("queued token");
    entry.may_open_window = true;
    let config = Arc::new(RankedMutex::new(app_config));
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::from([(
        "a".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 4.0,
                resets_at: Some(epoch_secs_to_iso(now_before + 3600)),
            }),
            ..UsageInfo::default()
        },
    )])));
    let refetch = Arc::new(RankedMutex::new(HashSet::new()));
    let activity = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks = Arc::new(RankedMutex::new(HashMap::new()));
    let blocks = Arc::new(RankedMutex::new(HashMap::from([(
        "a".to_string(),
        super::KickBlock {
            streak: 1,
            rejected: false,
            until: None,
            next_retry: now_before + 600,
        },
    )])));
    let queue = crate::usage::new_auto_start_queue_state();
    crate::usage::note_queue_open(&queue, &"a".into(), 42);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let _live = super::run_fetch(
        &config,
        entry.clone(),
        &store,
        &refetch,
        &activity,
        &streaks,
        &blocks,
        &queue,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(
        crate::usage::queue_anchor_cached(&queue),
        Some(42),
        "a successful live-window re-test opens nothing and cannot re-phase the queue"
    );
    assert!(
        lines
            .snapshot()
            .iter()
            .all(|line| !line.contains("5h auto-start window opened")),
        "the live-window re-test must not claim a queue open: {:?}",
        lines.snapshot()
    );

    store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some(epoch_secs_to_iso(now_before - 60)),
            }),
            ..UsageInfo::default()
        },
    );
    let lapsed_started = crate::usage::now_epoch_secs();
    let _lapsed = super::run_fetch(
        &config,
        entry,
        &store,
        &refetch,
        &activity,
        &streaks,
        &blocks,
        &queue,
        REFRESH_INTERVAL_MS,
    );
    let anchor = crate::usage::queue_anchor_cached(&queue).expect("landed lapsed kick anchor");
    assert!(
        anchor >= lapsed_started,
        "the anchor carries the landed kick's wall clock, got {anchor} before {lapsed_started}"
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("5h auto-start window opened"))
            .count(),
        1,
        "exactly the lapsed leg emits the queue-open event: {:?}",
        lines.snapshot()
    );

    let seen = server.join().expect("listener");
    assert_eq!(
        seen.iter()
            .filter(|path| path.starts_with("/v1/messages"))
            .count(),
        2,
        "both the health re-test and lapsed open reach the kick endpoint: {seen:?}"
    );
}

/// A failed elected kick is health state for that exact member. Keying the
/// streak on a sibling would let the failed head keep winning forever.
#[test]
fn auto_start_queue_run_fetch_keys_a_failed_kick_to_the_elected_member() {
    use crate::profile::{AppConfig, AppState};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};

    let home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_epoch_secs();
    let usage_body = format!(
        r#"{{"five_hour":{{"utilization":1.0,"resets_at":"{}"}}}}"#,
        epoch_secs_to_iso(now + 5 * 3600),
    );
    let (base, server) = crate::testutil::serve_endpoints(4, move |path, _| {
        if path.starts_with("/v1/messages") {
            (403, "{}".to_string())
        } else if path.starts_with("/api/oauth/usage") {
            (200, usage_body.clone())
        } else if path.starts_with("/api/oauth/profile") {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let app_config = AppConfig {
        state: AppState {
            fallback_chain: vec!["elected".into(), "sibling".into()],
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![
            auto_start_queue_profile("elected"),
            auto_start_queue_profile("sibling"),
        ],
    };
    let entry = super::collect_tokens(&app_config)
        .into_iter()
        .find(|entry| entry.name == "elected")
        .expect("elected token");
    let config = Arc::new(RankedMutex::new(app_config));
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::from([(
        "elected".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some(epoch_secs_to_iso(now - 60)),
            }),
            ..UsageInfo::default()
        },
    )])));
    let queue = crate::usage::new_auto_start_queue_state();
    let _outcome = super::run_fetch(
        &config,
        entry,
        &store,
        &Arc::new(RankedMutex::new(HashSet::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &queue,
        REFRESH_INTERVAL_MS,
    );
    let recorded_at = crate::usage::now_epoch_secs();
    assert_eq!(
        crate::usage::queue_failures(&queue, &"elected".into(), recorded_at),
        1,
        "the failed elected member owns the streak"
    );
    assert_eq!(
        crate::usage::queue_failures(&queue, &"sibling".into(), recorded_at),
        0,
        "the untouched sibling owns no failure"
    );

    let seen = server.join().expect("listener");
    assert_eq!(
        seen.iter()
            .filter(|path| path.starts_with("/v1/messages"))
            .count(),
        1,
        "the elected member made one failed kick attempt: {seen:?}"
    );
}

// ── active-profile 429 ladder cap ────────────────────────────────────────────
//
// A deep back-off slot on the active row mostly buys staleness on the exact
// row the user watches (2026-07-12: the endpoint recovered while the active
// account sat out a 14-minute slot as `RateLimited`), so shallow streaks cap
// at 2× cadence. The cap RELEASES past `ACTIVE_CAP_MAX_STREAK`: the `/usage`
// window counts rejected polls and only clauth's own polls fill it (#30), so
// a sustained storm must climb the same drain ladder as idle profiles or the
// capped re-polls keep the window pinned. Idle profiles always keep the full
// ladder.

#[test]
fn active_profile_rate_limit_ladder_caps_at_one_extra_interval() {
    use super::{IntervalMs, MAX_RETRY_AFTER_MS, next_slot_deferral};
    let interval = 90_000u64;
    // Deep streak: the idle ladder pushes the slot to the 15-min ceiling.
    assert_eq!(
        next_slot_deferral(true, None, 6, interval, false),
        IntervalMs::from_millis(MAX_RETRY_AFTER_MS - interval),
        "idle keeps the full drain ladder"
    );
    // Active: the slot lands at most one extra interval out (2x cadence).
    assert_eq!(
        next_slot_deferral(true, None, 6, interval, true),
        IntervalMs::from_millis(interval),
        "active caps at 2x cadence"
    );
}

#[test]
fn active_profile_cap_still_honors_a_real_server_hint() {
    use super::{IntervalMs, next_slot_deferral};
    let interval = 90_000u64;
    // A genuine long retry-after is a server directive, not ladder guesswork —
    // the active cap must not shorten it.
    assert_eq!(
        next_slot_deferral(
            true,
            Some(std::time::Duration::from_secs(600)),
            6,
            interval,
            true
        ),
        IntervalMs::from_millis(600_000 - interval),
        "a real retry-after wins over the active cap"
    );
}

#[test]
fn active_profile_cap_leaves_shallow_streaks_alone() {
    use super::next_slot_deferral;
    let interval = 90_000u64;
    // streak 1 ladder (interval + 10s) sits under the cap: identical either way.
    assert_eq!(
        next_slot_deferral(true, None, 1, interval, true),
        next_slot_deferral(true, None, 1, interval, false),
    );
}

/// Pins where the cap first bites and where it releases, so a drift in either
/// boundary fails loudly. At 90s cadence: streak 3's ladder (90s + 90s) equals
/// the 2× cap exactly (a no-op), streak 4 (90s + 270s) is the first capped
/// step, streak 6 the last, and streak 7 releases to the idle drain ladder.
#[test]
fn active_profile_cap_bites_at_streak_4_and_releases_past_6() {
    use super::{IntervalMs, MAX_RETRY_AFTER_MS, next_slot_deferral};
    let interval = 90_000u64;
    // streak 3: ladder == cap, active and idle agree.
    assert_eq!(
        next_slot_deferral(true, None, 3, interval, true),
        next_slot_deferral(true, None, 3, interval, false),
        "streak 3 sits exactly on the cap"
    );
    // streak 4: first bite — active holds 2x cadence, idle walks away.
    assert_eq!(
        next_slot_deferral(true, None, 4, interval, true),
        IntervalMs::from_millis(interval),
        "streak 4 is the first capped step"
    );
    assert_ne!(
        next_slot_deferral(true, None, 4, interval, false),
        IntervalMs::from_millis(interval),
        "idle streak 4 must not be capped"
    );
    // streak 7: the cap releases — the active row climbs the same drain
    // ladder as an idle profile (the sustained-storm concession).
    assert_eq!(
        next_slot_deferral(true, None, 7, interval, true),
        next_slot_deferral(true, None, 7, interval, false),
        "past the bound the active row must drain like an idle one"
    );
    assert_eq!(
        next_slot_deferral(true, None, 7, interval, true),
        IntervalMs::from_millis(MAX_RETRY_AFTER_MS - interval),
        "a released deep streak sits at the 15-min ceiling"
    );
}

/// The `is_active` flag threads through `apply_outcome` into the deferral —
/// a regression that drops or hardwires the flag stamps both rows the same.
/// Two profiles at the same deep streak, one active one idle: their stamped
/// next slots must differ by exactly the cap-vs-ladder gap.
#[test]
fn apply_outcome_threads_is_active_into_the_deferral() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome, now_ms};

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let statuses: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    // Both profiles arrive at streak 6 (deep, still capped for the active row).
    let at_five = super::StreakCounts {
        rate_limit: 5,
        refresh_fail: 0,
    };
    streaks.lock().unwrap().insert("act".to_string(), at_five);
    streaks.lock().unwrap().insert("idle".to_string(), at_five);

    let outcome = |name: &str| FetchOutcome {
        name: crate::profile::ProfileName::from(name),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: None,
    };

    let before = now_ms();
    apply_outcome(
        outcome("act"),
        &store,
        &statuses,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        true,
    );
    apply_outcome(
        outcome("idle"),
        &store,
        &statuses,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();

    let stamp = |name: &str| {
        last_fetched
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .expect("stamp present")
            .as_millis()
    };
    // Active at streak 6: capped → deferral = one extra interval.
    assert!(
        (before + REFRESH_INTERVAL_MS..=after + REFRESH_INTERVAL_MS).contains(&stamp("act")),
        "active stamp must carry the 2x-cadence cap"
    );
    // Idle at streak 6: full ladder → the 15-min ceiling.
    let idle_extra = super::MAX_RETRY_AFTER_MS - REFRESH_INTERVAL_MS;
    assert!(
        (before + idle_extra..=after + idle_extra).contains(&stamp("idle")),
        "idle stamp must carry the full drain ladder"
    );
}

// ── OAuth refresh-all: completion-ordered result processing ──────────────────
//
// Each due profile fetches on its own worker; result processing (spinner clear +
// countdown publish) must fire the instant that profile's OWN fetch resolves,
// keyed on completion order — not the `due` list order. The old join-in-list
// loop stalled a fast account's clear behind an earlier slow account, so a fast
// row's spinner stayed lit / its countdown hidden until the slow one ahead
// finished. This is the regression guard.

/// Build a `SchedulerState` whose two OAuth profiles are `slow` (listed first)
/// and `fast` (listed second) — the ordering that trips the join-order stall.
fn completion_order_state() -> super::SchedulerState {
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    super::SchedulerState {
        config: Arc::new(RankedMutex::new(AppConfig {
            state: AppState::default(),
            profiles: vec![],
        })),
        tokens: Arc::new(RankedMutex::new(vec![token("slow"), token("fast")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    }
}

/// A pure, disk-free outcome: `info: None` + `from_fetch: false` keeps
/// `apply_outcome` entirely in-memory (no cache read/write), so this test needs
/// no `HomeSandbox` and stays parallel-safe.
fn cached_outcome(name: &str) -> super::FetchOutcome {
    super::FetchOutcome {
        name: crate::profile::ProfileName::from(name),
        info: None,
        status: super::FetchStatus::Cached,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: None,
    }
}

#[test]
fn oauth_completions_apply_in_completion_order_not_list_order() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let state = completion_order_state();

    // `slow` (index 0) blocks in its worker until the test releases it; `fast`
    // (index 1) returns at once. The release is sent from a drop-guard at the end
    // of the scope, so even a failing assertion (RED) unblocks `slow` and lets
    // the scope join instead of hanging.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let worker = |entry: TokenEntry| -> super::FetchOutcome {
        if entry.name == "slow" {
            let _ = release_rx.lock().unwrap().recv();
        }
        cached_outcome(&entry.name)
    };

    /// Releases `slow` on drop so the scope always joins — success or panic.
    struct Release<'a>(&'a mpsc::Sender<()>);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    std::thread::scope(|scope| {
        // Dropped at closure end (success or panic), releasing `slow` so the
        // scope's implicit join never hangs.
        let _release = Release(&release_tx);
        scope.spawn(|| {
            super::fetch_oauth_due_with(
                &state,
                vec![token("slow"), token("fast")],
                REFRESH_INTERVAL_MS,
                worker,
            );
        });

        // Wait for `fast` to be fully applied (countdown published). In
        // completion order this happens within microseconds while `slow` is
        // still blocked; a list-order drain is stuck on `slow` and never applies
        // `fast`, so the deadline fires and the test fails RED.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state
            .next_refresh_per_profile
            .lock()
            .unwrap()
            .contains_key("fast")
        {
            assert!(
                Instant::now() < deadline,
                "`fast` was never applied while `slow` held the head of the list — \
                 result-processing is still gated on list order, not completion order"
            );
            std::thread::yield_now();
        }

        // The core guarantee: at the instant `fast` is applied, `slow` is still
        // pending (spinner mark intact, no countdown). So the later-listed fast
        // account resolved strictly ahead of the slow account before it.
        {
            let activity = state.activity.lock().unwrap();
            assert!(
                activity.get("fast").is_none(),
                "`fast` spinner cleared on its own completion"
            );
            assert!(
                matches!(activity.get("slow"), Some(ProfileActivity::Queued)),
                "`slow` is still queued — it did not gate `fast`"
            );
        }
        assert!(
            !state
                .next_refresh_per_profile
                .lock()
                .unwrap()
                .contains_key("slow"),
            "`slow` countdown is not yet published — it lands after `fast`"
        );
    });

    // Both profiles are fully applied once the batch drains. Read `activity`
    // (rank 600) before `next_refresh` (rank 1100) so the two reads honour the
    // global lock order even though they are logically independent here.
    assert!(
        state.activity.lock().unwrap().is_empty(),
        "every spinner cleared by batch end"
    );
    let nrpp = state.next_refresh_per_profile.lock().unwrap();
    assert!(
        nrpp.contains_key("fast") && nrpp.contains_key("slow"),
        "both countdowns published by batch end"
    );
}

// ── identity memo (adopt path) ───────────────────────────────────────────────
//
// A rotation tick can run two adopts, each resolving the stored and the live
// token's account uuid — up to four `/profile` GETs, 5s apart, for the same two
// immutable answers.
//
// The memo is process-lifetime, so both tests below take a `HomeSandbox` purely
// to serialize on `HOME_TEST_LOCK`: `EndpointSandbox` clears the memo on
// construction and drop, and under the `cargo test` fallback (threads, one
// process) an unserialized run would let that clear land between an insert here
// and the assertion on it.

/// A resolved uuid is fetched once per token: immutable, so a hit is exact.
/// Distinct tokens still each get their own probe.
#[test]
fn the_identity_memo_resolves_each_token_once() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::usage::reset_identity_memo();
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let probe = |tok: &str| {
        calls.borrow_mut().push(tok.to_string());
        Some(crate::profile::AccountId::from(format!("uuid-of-{tok}")))
    };
    let identity = memoized_identity(&probe);

    assert_eq!(identity("stored").as_deref(), Some("uuid-of-stored"));
    assert_eq!(
        identity("stored").as_deref(),
        Some("uuid-of-stored"),
        "the memo answers, and answers identically"
    );
    assert_eq!(identity("live").as_deref(), Some("uuid-of-live"));

    assert_eq!(
        calls.borrow().as_slice(),
        ["stored", "live"],
        "one probe per distinct token, no matter how often it is asked for"
    );
}

/// A failed probe must stay retryable. The adopt after a failed refresh exists
/// because the live mirror may have surfaced a fresh pair since the first
/// attempt — caching the `None` would silently make that second adopt a no-op.
#[test]
fn the_identity_memo_never_caches_a_failed_probe() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::usage::reset_identity_memo();
    let calls = std::cell::RefCell::new(0usize);
    // Fails the first time, succeeds after — the mirror catching up mid-tick.
    let probe = |_tok: &str| {
        *calls.borrow_mut() += 1;
        (*calls.borrow() > 1).then(|| crate::profile::AccountId::from("uuid-late"))
    };
    let identity = memoized_identity(&probe);

    assert_eq!(identity("live"), None, "first probe fails");
    assert_eq!(
        identity("live").as_deref(),
        Some("uuid-late"),
        "the retry must reach the probe, not a cached failure"
    );
    assert_eq!(*calls.borrow(), 2, "a None is never cached");

    assert_eq!(identity("live").as_deref(), Some("uuid-late"));
    assert_eq!(
        *calls.borrow(),
        2,
        "once it resolves, the answer is cached like any other"
    );
}

/// The memo outlives the call that filled it. Every `fetch_with_rotation` leg
/// builds its own closure, so a per-call store re-probes `/profile` on each one
/// — which is what a permanently-foreign live mirror turns into an unbounded
/// request stream. A per-call store passes both tests above and only this one.
#[test]
fn the_identity_memo_outlives_the_call_that_filled_it() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::usage::reset_identity_memo();
    let calls = std::cell::RefCell::new(0usize);
    let probe = |_tok: &str| {
        *calls.borrow_mut() += 1;
        Some(crate::profile::AccountId::from("uuid-durable"))
    };

    assert_eq!(
        memoized_identity(&probe)("tok-durable").as_deref(),
        Some("uuid-durable")
    );
    assert_eq!(
        memoized_identity(&probe)("tok-durable").as_deref(),
        Some("uuid-durable"),
        "a second leg's fresh closure must still answer from the memo"
    );
    assert_eq!(
        *calls.borrow(),
        1,
        "one probe per token per PROCESS, not per call"
    );
}

// ── scan_recovery ─────────────────────────────────────────────────────────
//
// The auto-recovery leg: after a switch-off-all, scans the fallback chain for
// a member back under its threshold and queues it. Every fixture below shares
// one chain member, "b", with a live 5h window under its default (95%)
// threshold — the shape that IS recoverable — and each test flips exactly one
// guard so the queue stays empty (or, for the happy path, fires).

use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

fn recoverable_store() -> UsageStore {
    Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            ..Default::default()
        },
    )])))
}

fn recovery_config(
    active_profile: Option<&str>,
    fallback_chain: &[&str],
) -> crate::profile::ConfigHandle {
    use crate::profile::{AppConfig, AppState, Profile};

    Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: active_profile.map(Into::into),
            fallback_chain: fallback_chain.iter().map(|s| (*s).into()).collect(),
            ..AppState::default()
        },
        profiles: vec![Profile::new("b".to_string(), None, None)],
    }))
}

/// A switch already queued means a previous decision (auto-switch or a prior
/// recovery scan) hasn't been dispatched yet; scanning again on top of it
/// could queue a second, contradictory switch.
#[test]
fn scan_recovery_is_a_no_op_while_a_switch_is_pending() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let store = recoverable_store();
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::from([
        "already-queued".to_string()
    ])));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert_eq!(
        pending.lock().unwrap().clone(),
        HashSet::from(["already-queued".to_string()]),
        "a pending switch must be left untouched, not joined by a second target"
    );
}

/// Recovery only applies to the switch-off-all state: an active profile means
/// there's nothing to relink.
#[test]
fn scan_recovery_is_a_no_op_with_an_active_profile_set() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(Some("a"), &["b"]),
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a live active profile must never be relinked over by a recovery scan"
    );
}

/// A `Cached`/`RateLimited`/absent read may be a rollover or a synthetic
/// just-kicked 0% — recovery must not relink to it even though its stored
/// numbers look recovered, matching the auto-switch side's freshness gate.
#[test]
fn scan_recovery_ignores_a_stale_or_synthetic_read() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let store = recoverable_store();
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));

    for stale in [
        FetchStatus::Cached,
        FetchStatus::RateLimited,
        FetchStatus::Failed,
    ] {
        let status: StatusStore =
            Arc::new(RankedMutex::new(HashMap::from([("b".to_string(), stale)])));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        scan_recovery(
            &recovery_config(None, &["b"]),
            &store,
            &status,
            &Arc::new(RankedMutex::new(HashMap::new())),
            &kick_blocks,
            &pending,
        );
        assert!(
            pending.lock().unwrap().is_empty(),
            "a {stale:?} read must not drive a recovery relink"
        );
    }

    // No read at all yet — same undecidable treatment.
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );
    assert!(
        pending.lock().unwrap().is_empty(),
        "an absent read is undecidable, not recovered"
    );
}

/// A switch-grade kick-rejected member is frozen by the messages-limiter, not
/// actually recovered — its idle-looking usage is exactly what the rejection
/// produces. Recovery must walk past it, never relink to it.
#[test]
fn scan_recovery_never_relinks_to_a_switch_grade_kick_rejected_member() {
    use super::{FetchStatus, KickBlock, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let store = recoverable_store();
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let now = now_epoch_secs();
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        KickBlock {
            streak: 2,
            rejected: true,
            until: Some(now + 600),
            next_retry: now + 30,
        },
    )])));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a switch-grade kick rejection must block recovery, not just auto-switch"
    );
}

/// The happy path: no pending switch, no active profile, a fresh read on a
/// chain member whose 5h window sits back under its threshold — queued.
#[test]
fn scan_recovery_queues_a_recovered_chain_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert_eq!(
        pending.lock().unwrap().clone(),
        HashSet::from(["b".to_string()]),
        "a recovered chain member must be queued for switch"
    );
}

/// Same recovered-usage shape as the happy path above, but the member is
/// disabled — the scan must never relink to it (mirrors the kick-rejected
/// exclusion just above).
#[test]
fn scan_recovery_never_relinks_to_a_disabled_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::profile::{AppConfig, AppState, Profile};

    let mut disabled_b = Profile::new("b".to_string(), None, None);
    disabled_b.disabled = true;
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: None,
            fallback_chain: vec!["b".into()],
            ..AppState::default()
        },
        profiles: vec![disabled_b],
    }));

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &config,
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a disabled member must never be relinked by the recovery scan"
    );
}

/// A chain name with no backing profile is not a relink target. The store can
/// still hold a live, recovered-looking entry under that name (a hand-edited
/// chain, or a profile deleted out from under one), and queueing it would
/// dispatch a switch to a profile that cannot be resolved. `walk_excluded`
/// drops it before the member list is built; without that term the entry below
/// reads exactly like the happy path and gets queued.
#[test]
fn scan_recovery_never_relinks_to_a_chain_member_with_no_profile() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::profile::{AppConfig, AppState, Profile};

    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: None,
            fallback_chain: vec!["ghost".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("b".to_string(), None, None)],
    }));

    // Same recovered shape the happy path queues on, keyed to the ghost name.
    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([(
        "ghost".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            ..Default::default()
        },
    )])));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "ghost".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &config,
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a chain member with no backing profile must never be relinked"
    );
}

/// Same recovered-usage shape as the happy path, but the member's plan reads
/// canceled — `/v1/messages` 403s no matter how idle its cached 5h window
/// looks, so it must never be a relink target (mirrors the disabled/kick-
/// rejected exclusions above; twin of the `fully_clear_target` canceled fix
/// on the target-side walk).
#[test]
fn scan_recovery_never_relinks_to_a_canceled_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::usage::{PlanInfo, PlanTier};

    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            plan: Some(PlanInfo {
                tier: PlanTier::Free,
                subscription_status: Some("canceled".to_string()),
            }),
            ..Default::default()
        },
    )])));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a canceled member must never be relinked by the recovery scan"
    );
}

/// Same recovered-usage shape as the happy path, but the member is flagged
/// auth-broken (AUTH-1 quarantine) — its store entry is frozen at the last
/// successful read while every refresh is permanently rejected, so it must
/// never be a relink target (mirrors the disabled exclusion above).
#[test]
fn scan_recovery_never_relinks_to_an_auth_broken_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::profile::{AppConfig, AppState, Profile};

    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: None,
            fallback_chain: vec!["b".into()],
            auth_broken: vec!["b".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("b".to_string(), None, None)],
    }));

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &config,
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "an auth-broken member must never be relinked by the recovery scan"
    );
}

/// `spawn_refresher`'s kick-block seed must run on the CALLING thread, not
/// inside the spawned tick worker: nothing joins that worker, so a home-
/// derived path resolved on it could outlive a test's `HOME_OVERRIDE` and read
/// the operator's real home — live the moment the seed grows a write leg.
/// Entering through
/// `spawn_refresher` itself (never `sync_kick_blocks_from_cache` directly) is
/// the only way to pin WHERE the seed runs; asserting immediately after return,
/// with no sleep or yield, is what makes the race decide against a broken
/// version instead of racing it.
#[test]
fn spawn_refresher_seeds_kick_blocks_before_returning() {
    use super::{KickBlock, KickBlocks, spawn_refresher};
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{KICK_BLOCK_CACHE_FILE, write_profile_cache};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let _home = crate::testutil::HomeSandbox::new();

    let cached = KickBlock {
        streak: 3,
        rejected: true,
        until: Some(1_700_000_600),
        next_retry: 1_700_000_100,
    };
    crate::testutil::register_names(&["kitty"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kitty"),
        KICK_BLOCK_CACHE_FILE,
        &cached,
    );

    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    }));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));

    spawn_refresher(
        config,
        Arc::new(RankedMutex::new(vec![token("kitty")])),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::clone(&kick_blocks),
        crate::usage::new_auto_start_queue_state(),
        Arc::new(RankedMutex::new(HashSet::new())),
        Arc::new(RankedMutex::new(false)),
        Arc::new(RankedMutex::new(HashSet::new())),
        Arc::new(RankedMutex::new(vec![])),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        // Pre-armed shutdown: if the `cfg!(test)` spawn-skip is ever removed
        // while the seed hoist stays, the tick thread this would spawn breaks
        // at its loop-top check instead of looping past this sandbox teardown.
        Arc::new(AtomicBool::new(true)),
        Arc::new(crate::daemon::FetchLease::new()),
    );

    // `cfg!(test)` makes `spawn_refresher` return without ever spawning the
    // tick thread, so the seed above is the ONLY thing that could have
    // populated `kick_blocks` — this is what pins the seed synchronous.
    assert_eq!(
        kick_blocks.lock().unwrap().get("kitty").copied(),
        Some(cached),
        "the on-disk kick block must be seeded before spawn_refresher returns"
    );
}

// ── Durable burn-rate history (usage_history.jsonl) ──────────────────────────
//
// The sample series behind BOTH burn readers: the TUI's in-memory
// `history_cache` and `fallback::burn_rate_for_profile`, the disk read that
// gates burn-aware auto-switching. It is appended on the FETCH path, so the
// holder of the single-fetcher lease owns it — a headless `clauth daemon` keeps
// it advancing with no TUI open, and no second process can interleave a line.
// Written from `App::apply_usage` instead, the log tracked TUI uptime: headless
// it froze, the 2-day prune then emptied it, and burn-aware auto-switch
// degraded to the static threshold indistinguishably from "no history yet".
//
// The seam: `apply_outcome` is where every fetch outcome lands, and it already
// holds both halves of a sample — `from_fetch` (the live-body gate the old TUI
// path spelled `FetchStatus::Fresh`) and the store entry the body replaces.

/// A 5h window at `utilization` with no reset stamp, so `preserve_live_window`
/// leaves the body untouched and the recorded sample is the fetched one.
fn history_sample(utilization: f64) -> crate::usage::UsageInfo {
    crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization,
            resets_at: None,
        }),
        ..Default::default()
    }
}

/// `(ts, 5h utilization)` pairs recorded for `name`, oldest first — read back
/// through the same parser both burn readers use.
fn recorded_samples(name: &str) -> Vec<(u64, f64)> {
    crate::profile::load_usage_history(&crate::profile::ProfileName::from(name))
        .into_iter()
        .filter_map(|(ts, info)| Some((ts, info.five_hour?.utilization)))
        .collect()
}

/// The four stores `apply_outcome` writes, empty.
fn history_stores() -> (
    super::UsageStore,
    super::StatusStore,
    LastFetchedAt,
    super::PollStreaks,
) {
    (
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
    )
}

/// A live fetch records the new sample AND bridges the one it replaces: the
/// previous value is re-stamped 1 ms earlier so an idle stretch keeps its
/// temporal density instead of replaying as one long ramp between two distant
/// points. Delete the `append_usage_sample` call in `apply_outcome` and the log
/// never appears at all — nothing else writes it.
#[test]
fn a_live_fetch_appends_the_sample_and_its_bridge() {
    use super::{FetchOutcome, apply_outcome};

    let _home = crate::testutil::HomeSandbox::new();
    let (store, status, last_fetched, streaks) = history_stores();
    // What the previous tick left in the store — the value the bridge carries.
    store
        .lock()
        .unwrap()
        .insert("alice".to_string(), history_sample(50.0));

    apply_outcome(
        FetchOutcome::live(
            &crate::profile::ProfileName::from("alice"),
            history_sample(80.0),
            None,
        ),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );

    let samples = recorded_samples("alice");
    assert_eq!(
        samples.len(),
        2,
        "a live fetch over a known previous value records the bridge plus the \
         new sample (got {samples:?})"
    );
    assert_eq!(
        samples[0].1, 50.0,
        "the bridge line carries the value being replaced, not the new one"
    );
    assert_eq!(
        samples[1].1, 80.0,
        "the live sample carries the fetched value"
    );
    assert_eq!(
        samples[1].0 - samples[0].0,
        1,
        "the bridge is stamped exactly 1 ms earlier so it never shares an \
         instant with the live sample"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path =
            crate::profile::profile_history_path(&crate::profile::ProfileName::from("alice"))
                .expect("history path");
        let mode = std::fs::metadata(&path)
            .expect("history metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the history log holds per-account utilization — owner-only, not umask"
        );
    }
}

/// A cached body — the recycled `usage_cache.json` snapshot a 429 or a network
/// failure falls back to — must record nothing. Its window may have rolled over
/// since, so a sample off it would write a phantom reset that survives restart
/// and skews the rate. Same gate the old `apply_usage` path spelled as
/// `FetchStatus::Fresh`; here it is `from_fetch`.
#[test]
fn a_cached_body_appends_no_sample() {
    use super::{FetchOutcome, FetchStatus, USAGE_CACHE_FILE, apply_outcome, write_profile_cache};

    let _home = crate::testutil::HomeSandbox::new();
    let (store, status, last_fetched, streaks) = history_stores();
    // A cached outcome loads its body off disk, so seed one to recycle.
    crate::testutil::register_names(&["alice"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("alice"),
        USAGE_CACHE_FILE,
        &history_sample(80.0),
    );

    let outcome = FetchOutcome::cached(
        &crate::profile::ProfileName::from("alice"),
        FetchStatus::RateLimited,
        None,
        None,
    );
    assert!(
        outcome.info.is_some(),
        "fixture must carry a body, or this asserts nothing about the gate"
    );
    apply_outcome(
        outcome,
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );

    assert!(
        recorded_samples("alice").is_empty(),
        "a recycled cached snapshot must not land a history sample"
    );
}

/// Two live fetches reading the same numbers record one sample, not two: the
/// series only grows when the numbers move. Without the dedup a quiet account
/// would fill the log with identical lines at the poll cadence.
#[test]
fn an_unchanged_live_sample_appends_nothing() {
    use super::{FetchOutcome, apply_outcome};

    let _home = crate::testutil::HomeSandbox::new();
    let (store, status, last_fetched, streaks) = history_stores();
    let apply = || {
        apply_outcome(
            FetchOutcome::live(
                &crate::profile::ProfileName::from("alice"),
                history_sample(80.0),
                None,
            ),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
            false,
        );
    };

    apply();
    let after_first = recorded_samples("alice");
    apply();

    assert_eq!(
        after_first.len(),
        1,
        "the first live sample over a cold store records itself and nothing to \
         bridge (got {after_first:?})"
    );
    assert_eq!(
        recorded_samples("alice"),
        after_first,
        "an identical second reading must leave the log untouched"
    );
}

/// Restart guard: a new process starts with an empty store, so the first live
/// fetch has no in-memory previous value to compare against. It must fall back
/// to the log's own last entry — otherwise every daemon restart re-appends the
/// sample already on disk, and the dedup above only holds within one process.
#[test]
fn a_cold_store_does_not_re_append_the_last_recorded_sample() {
    use super::{FetchOutcome, apply_outcome};

    let _home = crate::testutil::HomeSandbox::new();
    let (store, status, last_fetched, streaks) = history_stores();
    let apply = |util: f64| {
        apply_outcome(
            FetchOutcome::live(
                &crate::profile::ProfileName::from("alice"),
                history_sample(util),
                None,
            ),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
            false,
        );
    };

    apply(80.0);
    let before_restart = recorded_samples("alice");

    // The restart: the store the previous run built is gone, the log is not.
    store.lock().unwrap().clear();
    apply(80.0);

    assert_eq!(
        recorded_samples("alice"),
        before_restart,
        "the same reading after a restart must not duplicate the last line"
    );

    // A moved reading still records — and with no in-memory previous value it
    // records ONE line: bridging across the downtime would invent an anchor
    // over exactly the gap `BURN_GAP_CUT_MS` exists to cut.
    store.lock().unwrap().clear();
    apply(90.0);
    let after = recorded_samples("alice");
    assert_eq!(
        after.len(),
        before_restart.len() + 1,
        "a changed reading on a cold store records the sample alone, no bridge \
         over the downtime (got {after:?})"
    );
    assert_eq!(
        after.last().map(|(_, util)| *util),
        Some(90.0),
        "and it carries the new reading"
    );
}

/// Seed `name`'s history log with one sample 3 days old (past the retention
/// window) and one a minute old, and return the recent one for comparison.
///
/// `now` is the caller's clock, not this function's: seeding several profiles
/// from one base is what makes their `recent` stamps equal by construction. Read
/// per call, the disk write between two calls can cross a millisecond and the
/// stamps drift apart — a 12%-of-runs flake, not a theoretical one.
fn seed_stale_history(name: &str, now: u64) -> (u64, f64) {
    let path = crate::profile::profile_history_path(&crate::profile::ProfileName::from(name))
        .expect("history path");
    std::fs::create_dir_all(path.parent().expect("profile dir")).expect("create profile dir");
    let line = |ts: u64, util: f64| {
        format!(
            "{{\"ts\":{ts},\"name\":\"{name}\",\"usage\":{}}}\n",
            serde_json::to_string(&history_sample(util)).expect("sample serializes"),
        )
    };
    let stale = now - 3 * 24 * 60 * 60 * 1000;
    let recent = now - 60_000;
    std::fs::write(
        &path,
        format!("{}{}", line(stale, 10.0), line(recent, 40.0)),
    )
    .expect("seed history log");
    (recent, 40.0)
}

/// A profile carrying a history log, `disabled` or not. No credentials: what
/// makes a log prunable is the file on disk, not whether the profile is
/// currently pollable.
fn history_profile(name: &str, disabled: bool) -> crate::profile::Profile {
    let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    p.disabled = disabled;
    p
}

/// The startup leg of the retention trim runs on `spawn_refresher`'s CALLING
/// thread, next to the kick-block seed and for the same reason: it resolves a
/// home-derived path, and a path resolved on the never-joined tick thread could
/// outlive a test's `HOME_OVERRIDE` and rewrite a file in the operator's home.
///
/// Scope is every profile in config, NOT the poll work-list: `disabled` is
/// pinned here because `collect_tokens` filters it out, so a work-list-scoped
/// trim would silently leave that account's utilization history on disk forever.
#[test]
fn spawn_refresher_prunes_stale_history_before_returning() {
    use super::spawn_refresher;
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let _home = crate::testutil::HomeSandbox::new();

    // `kitty` polls; `sleepy` is disabled and `orphan` has no token entry at
    // all (creds dropped, or converted to an api-key base-url) — both still own
    // a log that has to age out. One clock for all three, so the expected stamp
    // below is a single value.
    let now = crate::usage::now_ms();
    let recent = seed_stale_history("kitty", now);
    seed_stale_history("sleepy", now);
    seed_stale_history("orphan", now);

    spawn_refresher(
        Arc::new(RankedMutex::new(AppConfig {
            state: AppState::default(),
            profiles: vec![
                history_profile("kitty", false),
                history_profile("sleepy", true),
                history_profile("orphan", false),
            ],
        })),
        Arc::new(RankedMutex::new(vec![token("kitty")])),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        crate::usage::new_auto_start_queue_state(),
        Arc::new(RankedMutex::new(HashSet::new())),
        Arc::new(RankedMutex::new(false)),
        Arc::new(RankedMutex::new(HashSet::new())),
        Arc::new(RankedMutex::new(vec![])),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        // Same pre-armed shutdown as the kick-block seed test: if the
        // `cfg!(test)` spawn-skip ever goes while this hoist stays, the tick
        // thread breaks at its loop top instead of outliving the sandbox.
        Arc::new(AtomicBool::new(true)),
        Arc::new(crate::daemon::FetchLease::new()),
    );

    for name in ["kitty", "sleepy", "orphan"] {
        assert_eq!(
            recorded_samples(name),
            vec![recent],
            "{name}: the 3-day-old sample must be gone and the recent one \
             intact by the time spawn_refresher returns"
        );
    }
}

/// The startup trim alone leaves the retention bound unenforced in the very
/// deployment this log exists for: a launchd/systemd daemon is built never to
/// restart, so nothing would trim after boot while the fetch path appends for
/// months — and `burn_rate_for_profile` re-parses the whole file from
/// `scan_auto_switch` on every 1 s tick. The cadenced leg is what bounds it.
#[test]
fn the_retention_trim_reruns_on_its_cadence_not_only_at_startup() {
    use super::{HISTORY_PRUNE_INTERVAL_MS, prune_histories_if_due};
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::AtomicU64;

    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    let recent = seed_stale_history("kitty", now);
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![history_profile("kitty", false)],
    }));

    // Startup pass just ran, so the log is left alone on the ticks in between.
    let last_prune = AtomicU64::new(now);
    assert!(
        !prune_histories_if_due(&last_prune, &config, now + HISTORY_PRUNE_INTERVAL_MS - 1),
        "a tick inside the cadence must not pay for a full read + rewrite"
    );
    assert_eq!(
        recorded_samples("kitty").len(),
        2,
        "and must leave the log untouched"
    );

    // The cadence elapses: the same long-running process trims without ever
    // having restarted.
    let due_at = now + HISTORY_PRUNE_INTERVAL_MS;
    assert!(
        prune_histories_if_due(&last_prune, &config, due_at),
        "the trim must run once the cadence has elapsed"
    );
    assert_eq!(
        recorded_samples("kitty"),
        vec![recent],
        "the stale sample is gone without a restart"
    );

    // The window is claimed, so the next tick is inside the cadence again.
    assert!(
        !prune_histories_if_due(&last_prune, &config, due_at + 1),
        "the run must reset the clock, not re-trim every tick from here on"
    );
}

// ── the rotation legs, driven offline ────────────────────────────────────────
//
// Every rotation decision sits BEHIND an HTTP call, so until `EndpointSandbox`
// existed a refusal deleted from `fetch_with_rotation` or `auto_start_kick` was
// caught by nothing at all (a mutation restoring one stayed green across the
// whole suite). These point all three Anthropic endpoints at one loopback
// listener and assert which of them a leg actually reaches.

/// THE ROW'S HEADLINE BEHAVIOUR. Under the old gate a live `clauth start`
/// session made this leg bail to disk cache on every 401, so such an account
/// served stale usage forever and never recovered its login. Off macOS it must
/// now rotate through the 401 and re-poll with the new token.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_401_under_a_live_session_rotates_and_retries() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "rot-live";
    // usage 401 (the dead token) → token 200 → usage 200 → profile 200 (the
    // plan pull that rides the post-rotation retry).
    let (base, server) = crate::testutil::serve_endpoints(6, |path, i| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/usage") {
            // Only the FIRST poll is rejected; the retry runs on the new token.
            if i == 0 {
                (401, r#"{"error":"unauthorized"}"#.to_string())
            } else {
                (
                    200,
                    r#"{"limits":[{"kind":"session","percent":12,
                   "resets_at":"2099-01-01T00:00:00+00:00"}]}"#
                        .to_string(),
                )
            }
        } else if path.starts_with("/api/oauth/profile") {
            (200, r#"{"account":{"uuid":"uuid-1"}}"#.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let _pid = {
        let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("profile dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir sessions");
        let f = crate::runtime::open_pid_file(&sessions.join("99999")).expect("pid");
        f.lock().expect("lock pid");
        f
    };
    assert!(
        crate::runtime::has_live_session(&crate::profile::ProfileName::from(name)),
        "fixture must actually read as live, or this proves nothing"
    );

    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let entry = super::TokenEntry {
        name: crate::profile::ProfileName::from(name),
        access_token: "at-old".into(),
        refresh_token: Some("rt-old".into()),
        auto_start: false,
        access_expires_at: Some(crate::usage::now_ms() as i64 + 86_400_000),
        auth_broken: false,
        may_open_window: true,
    };
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the leg must REACH the token endpoint — a live session no longer bails: {seen:?}"
    );
    assert_eq!(
        outcome.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "the rotated pair must come back for the TokenList sync"
    );
    #[allow(clippy::expect_used, reason = "test")]
    let stored = config
        .lock()
        .expect("config lock")
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(stored.as_deref(), Some("at-new"), "the pair persisted");
    // The whole point: the account is serving LIVE usage again, not the stale
    // disk cache the old gate pinned it to forever.
    assert_eq!(outcome.status, super::FetchStatus::Fresh);
    assert!(
        outcome.from_fetch,
        "a live body, not a cache fallback: {:?}",
        outcome.status
    );
}

/// `auto_start_kick`'s refusal sits AFTER its first kick, so only a real
/// listener reaches it. Off macOS a 401 kick on a live-session account must
/// rotate and retry the kick rather than giving up.
#[cfg(not(target_os = "macos"))]
#[test]
fn auto_start_kick_rotates_under_a_live_session() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "kick-live";
    // messages 401 → token 200 → messages 200
    let (base, server) = crate::testutil::serve_endpoints(5, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else if path.starts_with("/v1/messages") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let _pid = {
        let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("profile dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir sessions");
        let f = crate::runtime::open_pid_file(&sessions.join("99999")).expect("pid");
        f.lock().expect("lock pid");
        f
    };
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));

    let result = crate::oauth::auto_start_kick(
        &config,
        &crate::profile::ProfileName::from(name),
        "at-old",
        Some("rt-old"),
        None,
        None,
    );
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the kick's rotation leg must run under a live session: {seen:?}"
    );
    assert_eq!(
        result.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "a minted pair must always propagate to the caller"
    );
}

/// The macOS counterpart: the same 401, the same live session, and the leg must
/// NOT reach the token endpoint. This is the sign-out the refusal exists to
/// prevent — clauth cannot hand the rotated pair to that session's Claude Code,
/// so spending the chain would strand it. Serving cache here is the correct
/// outcome, not a degradation.
#[cfg(target_os = "macos")]
#[test]
fn a_401_under_a_live_session_does_not_rotate_on_macos() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "rot-live-mac";
    let (base, server) = crate::testutil::serve_endpoints(3, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let _pid = {
        let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("profile dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir sessions");
        let f = crate::runtime::open_pid_file(&sessions.join("99999")).expect("pid");
        f.lock().expect("lock pid");
        f
    };
    assert!(
        crate::runtime::has_live_session(&crate::profile::ProfileName::from(name)),
        "fixture must read live"
    );

    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let entry = super::TokenEntry {
        name: crate::profile::ProfileName::from(name),
        access_token: "at-old".into(),
        refresh_token: Some("rt-old".into()),
        auto_start: false,
        access_expires_at: Some(crate::usage::now_ms() as i64 + 86_400_000),
        auth_broken: false,
        may_open_window: true,
    };
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        !seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "macOS must not spend the chain a live session holds: {seen:?}"
    );
    assert_eq!(outcome.rotated, None, "nothing may be rotated");
    #[allow(clippy::expect_used, reason = "test")]
    let stored = config
        .lock()
        .expect("config lock")
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(
        stored.as_deref(),
        Some("at-old"),
        "the stored pair is untouched"
    );
}

/// The macOS counterpart for the kick leg: a 401 kick on a live-session account
/// must stop at the kick, never rotate.
#[cfg(target_os = "macos")]
#[test]
fn auto_start_kick_does_not_rotate_under_a_live_session_on_macos() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "kick-live-mac";
    let (base, server) = crate::testutil::serve_endpoints(3, |path, _| {
        if path.starts_with("/v1/messages") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let _pid = {
        let sessions = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("profile dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir sessions");
        let f = crate::runtime::open_pid_file(&sessions.join("99999")).expect("pid");
        f.lock().expect("lock pid");
        f
    };
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));

    let result = crate::oauth::auto_start_kick(
        &config,
        &crate::profile::ProfileName::from(name),
        "at-old",
        Some("rt-old"),
        None,
        None,
    );
    let seen = server.join().expect("listener");

    assert!(
        !seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "macOS must not rotate the chain a live session holds: {seen:?}"
    );
    assert_eq!(result.rotated, None, "nothing may be rotated");
    assert!(
        !result.opened,
        "the window stays shut; the kick could not recover"
    );
}

// ── the rest of `fetch_with_rotation`, driven offline ────────────────────────
//
// Everything below the 401 arm above: the clock-expired-429 unmask, both retry
// failure arms, the guard-acquire bail, and the persist-failure carry-back. Each
// sits behind at least one HTTP round trip, so the loopback listener is what
// makes any of them reachable.

/// The rotation fixture with the stored access token ALREADY clock-expired and
/// preemptive rotation OFF (the Config-tab escape hatch). Both halves are what
/// put a `/usage` 429 on the AUTH-1 unmask path: with the toggle on, that same
/// expiry takes the proactive branch and the plain fetch the 429 comes from
/// never runs at all.
///
/// Gated with its callers: every consumer below is a non-macOS test, and an
/// ungated helper with no macOS caller is a dead-code error that reds that leg
/// on clippy `-D warnings` before a test runs.
#[cfg(not(target_os = "macos"))]
fn expired_lazy_config(name: &str) -> crate::profile::ConfigHandle {
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    {
        let mut cfg = config.lock().unwrap();
        cfg.state.preemptive_rotation = false;
        let oauth = cfg
            .find_mut(&crate::profile::ProfileName::from(name))
            .and_then(|p| p.credentials.as_mut())
            .and_then(|c| c.claude_ai_oauth.as_mut())
            .expect("the fixture profile carries an OAuth block");
        oauth.expires_at = Some(crate::usage::now_ms() as i64 - 1_000);
    }
    config
}

fn rotation_entry(name: &str, access_expires_at: Option<i64>) -> super::TokenEntry {
    super::TokenEntry {
        name: crate::profile::ProfileName::from(name),
        access_token: "at-old".into(),
        refresh_token: Some("rt-old".into()),
        auto_start: false,
        access_expires_at,
        auth_broken: false,
        may_open_window: true,
    }
}

/// Anchor the profile and stamp its `/profile` clock fresh, so `take_profile_fetch`
/// refuses on its own and the ONLY thing that can still pull `/profile` is
/// `force_profile`. Without this the first attempt spends the hourly slot itself
/// and the force is invisible.
fn silence_profile_ttl(name: &str) {
    use crate::profile_cache::{
        ACCOUNT_ID_CACHE_FILE, PROFILE_FETCHED_CACHE_FILE, write_profile_cache,
    };
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        ACCOUNT_ID_CACHE_FILE,
        &crate::profile::AccountId::from("uuid-anchor"),
    );
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        PROFILE_FETCHED_CACHE_FILE,
        &crate::usage::now_ms(),
    );
}

/// A disk cache to fall back onto, so a bail's status is the one the leg chose
/// rather than the `Failed` downgrade `load_cached_with_status` applies when
/// there is nothing cached at all.
fn seed_usage_cache(name: &str) {
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );
}

/// Write the live slot (`~/.claude/.credentials.json`) as a REGULAR file holding
/// `access` — what CC leaves behind every time it refreshes, since it renames a
/// temp sibling over the destination and `rename(2)` acts on the link rather
/// than its target. Against the fixture's stored `at-old` this classifies
/// `LinkState::Diverged`, the adopt's second gate.
fn write_live_mirror(access: &str, expires_at: i64) {
    let live = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("claude dir parent")).expect("mkdir claude dir");
    let creds = crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("{access}-refresh")),
            expires_at: Some(expires_at),
            scopes: None,
            subscription_type: None,
        }),
    };
    std::fs::write(&live, serde_json::to_vec(&creds).expect("serialize mirror"))
        .expect("write the live mirror");
}

/// A `/profile` body answering the uuid `silence_profile_ttl` anchors the
/// profile to, so the live mirror's token proves the SAME account.
const ANCHOR_PROFILE_BODY: &str = r#"{"account":{"uuid":"uuid-anchor"}}"#;

/// A `/profile` body for an account whose subscription was canceled — the tier
/// flip that only the live-token re-pull can observe.
#[cfg(not(target_os = "macos"))]
const CANCELED_PROFILE_BODY: &str = r#"{"account":{"uuid":"uuid-1"},
   "organization":{"organization_type":"claude_free","subscription_status":"canceled"}}"#;

#[cfg(not(target_os = "macos"))]
fn active_pro_plan() -> crate::usage::PlanInfo {
    crate::usage::PlanInfo {
        tier: crate::usage::PlanTier::Pro,
        subscription_status: Some("active".to_string()),
    }
}

/// The AUTH-1 unmask end to end: a `/usage` 429 on an already clock-expired token
/// is not an endpoint throttle to sit out — it masks a token that must be
/// refreshed, and refusing to rotate leaves a dead login hiding behind
/// `RateLimited` forever. It also pins the `unmask_429` half of `force_profile`:
/// the 429'd attempt already spent this tick's hourly `/profile` slot on the
/// now-dead token, so without the force the canceled account below would keep
/// reporting the previous tier for a full hour.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_429_on_a_clock_expired_token_rotates_and_repulls_the_plan() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "unmask-429";
    // usage 429 → token 200 → usage 200 → profile 200. `max` sits above that, so
    // a second `/profile` (or any extra request) would still be recorded.
    let (base, server) = crate::testutil::serve_endpoints(7, |path, i| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/usage") {
            if i == 0 {
                (429, r#"{"error":"rate_limited"}"#.to_string())
            } else {
                (
                    200,
                    r#"{"limits":[{"kind":"session","percent":12,
                   "resets_at":"2099-01-01T00:00:00+00:00"}]}"#
                        .to_string(),
                )
            }
        } else if path.starts_with("/api/oauth/profile") {
            (200, CANCELED_PROFILE_BODY.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = expired_lazy_config(name);
    silence_profile_ttl(name);
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 - 1_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    // A plan already in hand, so `prev_plan.is_none()` is FALSE and the unmask is
    // the only thing left that can force the retry's `/profile` pull.
    let outcome = super::fetch_with_rotation(
        &config,
        &entry,
        Some(active_pro_plan()),
        &refetch,
        &activity,
    );
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "a 429 on a clock-expired token must reach the token endpoint: {seen:?}"
    );
    assert_eq!(
        outcome.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "the rotated pair must come back for the TokenList sync"
    );
    assert_eq!(outcome.status, super::FetchStatus::Fresh);
    assert!(outcome.from_fetch, "a live body, not a cache fallback");
    let profile_calls = seen
        .iter()
        .filter(|p| p.starts_with("/api/oauth/profile"))
        .count();
    assert_eq!(
        profile_calls, 1,
        "the fresh token must re-pull /profile past the hourly stamp: {seen:?}"
    );
    let plan = outcome
        .info
        .as_ref()
        .and_then(|i| i.plan.as_ref())
        .expect("the live body carries a plan");
    assert!(
        plan.is_canceled(),
        "the re-pull is what observes the cancellation; got {plan:?}"
    );
    assert_eq!(plan.tier, crate::usage::PlanTier::Free);
}

/// The post-rotation retry itself 429s. The minted pair still comes back (the old
/// single-use token is already spent), the `/profile` reading taken despite that
/// 429 rides along so a canceled account is still observed — and the profile must
/// NOT be pushed onto the refetch queue: enqueueing here is the
/// rotate → 429 → enqueue → rotate cycle the `retry-after` deferral exists to
/// replace.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_rate_limited_retry_keeps_the_pair_and_never_enqueues_a_refetch() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "retry-429";
    // usage 401 → token 200 → usage 429 → profile 200.
    let (base, server) = crate::testutil::serve_endpoints(7, |path, i| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/usage") {
            if i == 0 {
                (401, r#"{"error":"unauthorized"}"#.to_string())
            } else {
                (429, r#"{"error":"rate_limited"}"#.to_string())
            }
        } else if path.starts_with("/api/oauth/profile") {
            (200, CANCELED_PROFILE_BODY.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    seed_usage_cache(name);
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 + 86_400_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the 401 must rotate before the retry can be rate-limited: {seen:?}"
    );
    assert_eq!(
        outcome.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "a rate-limited retry still holds the only usable pair"
    );
    assert_eq!(outcome.status, super::FetchStatus::RateLimited);
    assert!(!outcome.from_fetch, "the 429 serves the disk cache");
    assert!(
        refetch.lock().unwrap().is_empty(),
        "a 429 defers by retry-after; forcing it back onto the next tick re-rotates"
    );
    let plan = outcome
        .plan_override
        .as_ref()
        .expect("the /profile reading taken despite the 429 rides along");
    assert!(plan.is_canceled(), "got {plan:?}");
}

/// Rotation succeeded but the retry died on a transient error. The pair comes
/// back AND the profile is queued so the next tick re-polls with the new token
/// instead of sitting out a full refresh interval on stale cache.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_failed_retry_keeps_the_pair_and_queues_a_refetch() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "retry-transient";
    // usage 401 → token 200 → usage 500. `/profile` is never reached: the
    // non-429 error arm short-circuits before the plan leg.
    let (base, server) = crate::testutil::serve_endpoints(6, |path, i| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/usage") {
            if i == 0 {
                (401, r#"{"error":"unauthorized"}"#.to_string())
            } else {
                (500, r#"{"error":"server_error"}"#.to_string())
            }
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    seed_usage_cache(name);
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 + 86_400_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the 401 must rotate before the retry can fail: {seen:?}"
    );
    assert_eq!(
        outcome.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "the minted pair survives a failed retry"
    );
    assert_eq!(outcome.status, super::FetchStatus::Cached);
    assert!(!outcome.from_fetch);
    assert_eq!(
        refetch.lock().unwrap().iter().collect::<Vec<_>>(),
        vec![&name.to_string()],
        "the new token must be retried next tick, not one interval from now"
    );
}

/// The rotation lock is unavailable, so the leg may not touch the credentials at
/// all: it bails to cache without reaching the token endpoint. Forced by putting
/// a DIRECTORY at the path `rotation_lock_path` returns, which is `EISDIR` when
/// `open_pid_file` opens it read-write — one of the IO failures `acquire`
/// returns `Err` for in production.
#[cfg(not(target_os = "macos"))]
#[test]
fn an_unacquirable_rotation_guard_bails_without_spending_the_chain() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "guard-unacquirable";
    // Zero token requests expected; `max` is above the one `/usage` call so a
    // leaked rotation would be recorded rather than silently refused a socket.
    let (base, server) = crate::testutil::serve_endpoints(4, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    seed_usage_cache(name);
    let lock_path = crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from(name))
        .expect("lock path");
    std::fs::create_dir_all(&lock_path).expect("block the rotation lock");
    assert!(
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name)).is_err(),
        "the fixture must actually deny the guard, or this proves nothing"
    );
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 + 86_400_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        !seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "an unheld guard must never let the single-use chain be spent: {seen:?}"
    );
    assert_eq!(outcome.rotated, None, "nothing may be rotated");
    assert_eq!(outcome.status, super::FetchStatus::Cached);
    let stored = config
        .lock()
        .unwrap()
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(
        stored.as_deref(),
        Some("at-old"),
        "the stored pair is untouched"
    );
}

/// A rotation whose persist fails still hands the minted pair back. The refresh
/// already spent the old single-use token, so the new pair is the only usable
/// one: dropping it leaves the caller's `TokenList` on a dead token that 400s
/// every tick until a restart adopts the staged sidecar, while the in-memory
/// `AppConfig` has already moved on — the divergence asserted below.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_rotation_carries_its_pair_back_when_the_persist_fails() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "persist-fail";
    // usage 401 → token 200; the retry never runs (the persist bails first).
    let (base, server) = crate::testutil::serve_endpoints(5, |path, i| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else if path.starts_with("/api/oauth/usage") && i == 0 {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    seed_usage_cache(name);
    crate::testutil::block_credentials_write(&crate::profile::ProfileName::from(name));
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 + 86_400_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the leg must actually rotate before the persist can fail: {seen:?}"
    );
    // Proof the fixture failed the persist where it claims to: the crash-durable
    // sidecar is only cleared after a committed save.
    let pending = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    assert!(
        pending.is_file(),
        "the staged sidecar must survive, or the save never failed"
    );
    assert_eq!(
        outcome.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "the pair the refresh minted is the only usable one — it must reach the TokenList"
    );
    assert_eq!(outcome.status, super::FetchStatus::Cached);
    assert!(!outcome.from_fetch);
    let stored = config
        .lock()
        .unwrap()
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(
        stored.as_deref(),
        Some("at-new"),
        "in-memory config already advanced — dropping the pair is what strands the TokenList"
    );
}

// ── the two adopt CALL SITES ─────────────────────────────────────────────────
//
// `try_adopt_live_rotation` itself is covered in `tests/inline/oauth.rs`; what
// was covered by nothing is `fetch_with_rotation` REACHING it. Both sites sat
// behind a `keychain_live()` term that is `false` under `cfg(test)` on macOS and
// hardcoded `false` everywhere else, so no test on any platform could enter
// them. Deleting that term is what these two pin.
//
// Ungated on purpose, unlike their neighbours above: both call sites sit BEFORE
// `rotation_blocked_for`, the macOS live-session refusal, and neither fixture
// arms a live session. The one platform-conditional step inside the adopt is
// the post-adopt relink, which no assertion here reads.

/// The pre-spend adopt. CC's routine refresh renames a fresh regular file over
/// clauth's symlink, so the live slot holds a fresher same-account pair while
/// the store lags — and the store's refresh token is already spent. Adopting
/// costs zero requests to the token endpoint; spending is what lands the
/// profile in `auth_broken` on the next tick.
#[test]
fn a_fresher_live_mirror_is_adopted_before_any_refresh_is_spent() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "adopt-presspend";
    // usage 401 → profile 200 (the live token's identity probe). `max` sits well
    // above that, so a token request the leg must NOT make is still recorded.
    let (base, server) = crate::testutil::serve_endpoints(5, |path, _| {
        if path.starts_with("/api/oauth/usage") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else if path.starts_with("/api/oauth/profile") {
            (200, ANCHOR_PROFILE_BODY.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    seed_usage_cache(name);
    silence_profile_ttl(name);
    // Strictly later than the fixture's stored expiry — the adopt's third gate.
    write_live_mirror("at-mirror-pre", crate::usage::now_ms() as i64 + 90_000_000);
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 + 86_400_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        !seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "adopting a pair CC already minted must spend nothing: {seen:?}"
    );
    assert_eq!(
        outcome.rotated,
        Some((
            "at-mirror-pre".to_string(),
            Some("at-mirror-pre-refresh".to_string())
        )),
        "the adopted pair must reach the TokenList, or the next poll spends the revoked one"
    );
    let stored = config
        .lock()
        .unwrap()
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(
        stored.as_deref(),
        Some("at-mirror-pre"),
        "the store caught up"
    );
    assert_eq!(outcome.status, super::FetchStatus::Cached);
    assert_eq!(
        refetch.lock().unwrap().iter().collect::<Vec<_>>(),
        vec![&name.to_string()],
        "this tick serves cache; the next one polls with the adopted token"
    );
}

/// The retry adopt, after the refresh comes back terminally rejected. The
/// mirror surfaces its fresher pair DURING that round trip — the race the
/// second call site exists for — so the leg adopts instead of quarantining.
/// Without the adopt, `carry_external_rotation` reads a store nothing advanced
/// and `mark_auth_broken` fires, which only a login, a carry, or an adopt lifts.
#[test]
fn an_adopt_after_a_rejected_refresh_keeps_the_profile_out_of_quarantine() {
    let home = crate::testutil::HomeSandbox::new();
    let name = "adopt-retry";
    let mirror_expiry = crate::usage::now_ms() as i64 + 90_000_000;
    // usage 401 → token 400 `invalid_grant` → profile 200. The mirror is written
    // as the token request is served, so the FIRST adopt sees a missing live
    // slot and only the retry can succeed.
    let (base, server) = crate::testutil::serve_endpoints(6, move |path, _| {
        if path.starts_with("/v1/oauth/token") {
            write_live_mirror("at-mirror-retry", mirror_expiry);
            (400, r#"{"error":"invalid_grant"}"#.to_string())
        } else if path.starts_with("/api/oauth/usage") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else if path.starts_with("/api/oauth/profile") {
            (200, ANCHOR_PROFILE_BODY.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    seed_usage_cache(name);
    silence_profile_ttl(name);
    let entry = rotation_entry(name, Some(crate::usage::now_ms() as i64 + 86_400_000));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let activity: super::ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = super::fetch_with_rotation(&config, &entry, None, &refetch, &activity);
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the refresh must actually be attempted and rejected, or the retry never runs: {seen:?}"
    );
    assert!(
        !config
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from(name)),
        "the adopted pair proves the chain is alive — quarantining here needs a manual login"
    );
    assert_eq!(
        outcome.rotated,
        Some((
            "at-mirror-retry".to_string(),
            Some("at-mirror-retry-refresh".to_string())
        )),
        "the adopted pair must reach the TokenList"
    );
    let stored = config
        .lock()
        .unwrap()
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(stored.as_deref(), Some("at-mirror-retry"));
    assert_eq!(outcome.status, super::FetchStatus::Cached);
}

/// CLA-ROLL: a rolling-token profile forces the preemptive leg regardless of the global
/// toggle, because here the rotation is not only about the chain — its persist
/// re-stamps the fed session token, and a stale sidecar has a live claude
/// reading it.
///
/// The rolling-token axis ORs over `enabled` and NOTHING else. The last two rows are the
/// ones that earn their keep: letting `feed` short-circuit the whole predicate
/// (`feed || expiry_inside_window`) survives every other assertion in this file,
/// so without them the conjunct is unpinned on exactly the axis this feature
/// adds.
#[test]
fn rolling_token_forces_the_preemptive_leg() {
    let interval = 90_000u64;
    let lead = super::rotate_lead_ms(interval);
    // Toggle OFF + feed ON → rotates inside the lead window anyway.
    assert!(super::proactive_rotation_due(
        false,
        true,
        Some(10_000 + lead),
        10_000,
        interval
    ));
    // Both off → inert, the stock pre-stamp behavior.
    assert!(!super::proactive_rotation_due(
        false,
        false,
        Some(10_000),
        10_000,
        interval
    ));
    // Feed OFF + toggle ON keeps the pre-stamp contract byte-for-byte.
    assert!(super::proactive_rotation_due(
        true,
        false,
        Some(10_000 + lead),
        10_000,
        interval
    ));
    // ── the conjunct, pinned on the rolling-token axis ──
    // A rolling-token profile BEYOND the lead window still waits its turn.
    assert!(!super::proactive_rotation_due(
        false,
        true,
        Some(10_000 + lead + 1),
        10_000,
        interval
    ));
    // A rolling-token profile whose expiry we cannot prove never spends a single-use
    // refresh — the rule in the doc comment holds on this axis too.
    assert!(!super::proactive_rotation_due(
        false, true, None, 10_000, interval
    ));
}

fn rolling_profile_config(
    rolling_names: &[&str],
    plain_names: &[&str],
) -> crate::profile::ConfigHandle {
    let profiles = rolling_names
        .iter()
        .map(|n| {
            let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from(*n));
            p.rolling_token = true;
            p
        })
        .chain(
            plain_names
                .iter()
                .map(|n| crate::testutil::blank_profile(&crate::profile::ProfileName::from(*n))),
        )
        .collect();
    Arc::new(RankedMutex::new(crate::profile::AppConfig {
        state: crate::profile::AppState {
            profiles: rolling_names
                .iter()
                .chain(plain_names.iter())
                .map(|n| (*n).into())
                .collect(),
            ..Default::default()
        },
        profiles,
    }))
}

/// A rolling (refresh-less) sidecar expiring `exp_in_ms` from now. The chain
/// grant carries a scope beyond the setup pair, like every real usage chain —
/// `stamp_rolling_token` refuses anything less by design.
fn write_rolling_sidecar(name: &str, exp_in_ms: i64) {
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &crate::profile::OAuthToken {
            access_token: format!("{name}-rolled"),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + exp_in_ms),
            scopes: Some(vec!["user:inference".into(), "user:profile".into()]),
            subscription_type: None,
        },
    )
    .expect("stamp rolling sidecar");
}

/// A dying fed bearer gets the gate; a second tick inside the scan gap does
/// not re-run it.
#[test]
fn claude_rolling_tick_restamps_a_dying_rolling_sidecar() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-feed"], &[]);
    write_rolling_sidecar("cl-feed", 60 * 60 * 1000); // +1h, inside the 2h horizon
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    super::claude_rolling_tick(&config, &pacing, now, &|name| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(name, "cl-feed");
        crate::oauth::AuthGate::Ready
    });
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the scan gap: no re-run even though the sidecar is still dying.
    super::claude_rolling_tick(&config, &pacing, now + 1_000, &|_| {
        panic!("inside the scan gap — must not re-run")
    });
}

/// Fresh sidecars and non-rolling profiles are never the timer's business.
#[test]
fn claude_rolling_tick_ignores_fresh_and_non_rolling_profiles() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-fresh"], &["cl-plain"]);
    write_rolling_sidecar("cl-fresh", 6 * 60 * 60 * 1000); // clear of the horizon
    write_rolling_sidecar("cl-plain", 60 * 60 * 1000); // dying, but the rolling token is OFF
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    super::claude_rolling_tick(&config, &pacing, crate::usage::now_ms(), &|name| {
        panic!("'{name}' must not be re-stamped")
    });
}

/// A Ready that leaves the sidecar STILL due (the degrade leg serving a live
/// mint/bearer through transient chain trouble) paces like a transient — no
/// per-scan re-run of the gate.
#[test]
fn claude_rolling_tick_ready_but_still_due_paces_like_transient() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-degrade"], &[]);
    write_rolling_sidecar("cl-degrade", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let degrading = |_: &crate::profile::ProfileName| {
        // Ready without advancing the sidecar — the degrade posture.
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Ready
    };
    super::claude_rolling_tick(&config, &pacing, now, &degrading);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Re-open the scan gate: the still-due Ready must have widened.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(&config, &pacing, now + 60_000, &degrading);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a degrade-masked Ready paces like a transient, not per scan"
    );

    // Past the widening → retried.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 1,
        &degrading,
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A transient gate failure widens the per-profile retry past the scan gap.
#[test]
fn claude_rolling_tick_transient_failure_widens_the_retry() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-flaky"], &[]);
    write_rolling_sidecar("cl-flaky", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let flaky = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Transient(crate::format::Transient::new(
            crate::format::Cause::Endpoint("connection reset"),
            crate::format::Retry::Connection,
        ))
    };
    super::claude_rolling_tick(&config, &pacing, now, &flaky);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Re-open the scan gate; the per-profile widening must still hold.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(&config, &pacing, now + 60_000, &flaky);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "transient failure widens past the scan cadence"
    );

    // Past the widening → retried.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(&config, &pacing, now + super::ROLLING_RETRY_MS + 1, &flaky);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A DISABLED profile is off every operational surface by definition, and this
/// leg can reach a guarded refresh — so sourcing candidates from the raw
/// profile list instead of `enabled_profiles()` spends single-use refresh
/// tokens, every 5 minutes, on accounts the operator took out of service.
#[test]
fn claude_rolling_tick_skips_a_disabled_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-off"], &[]);
    // Disable it the way `clauth disable` does.
    {
        let mut cfg = config.lock().expect("lock");
        cfg.find_mut(&crate::profile::ProfileName::from("cl-off"))
            .expect("profile")
            .disabled = true;
    }
    write_rolling_sidecar("cl-off", 60 * 60 * 1000); // dying, inside the horizon
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    super::claude_rolling_tick(&config, &pacing, crate::usage::now_ms(), &|name| {
        panic!("'{name}' is disabled and must never reach the re-stamp leg")
    });
}

/// A standing `auth_broken` takes the Broken leash on EVERY verdict. Without
/// it, a quarantined chain whose sidecar is running out its clock lands in the
/// Ready-still-due arm and picks up a second 15-minute cadence on top of the
/// poll leg's own backoff — bounded, but retrying a roll that
/// `roll_from_stored_chain` routes to `ChainStale` by flag every time.
#[test]
fn claude_rolling_tick_quarantined_chain_takes_the_broken_leash() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-quarantined"], &[]);
    config
        .lock()
        .expect("lock")
        .state
        .auth_broken
        .push("cl-quarantined".into());
    write_rolling_sidecar("cl-quarantined", 60 * 60 * 1000); // dying
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let degrading = |_: &crate::profile::ProfileName| {
        // Ready without advancing the sidecar — the degrade posture a dead
        // chain produces (restore false / bearer still serving).
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Ready
    };
    super::claude_rolling_tick(&config, &pacing, now, &degrading);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Where an unflagged profile would retry (past ROLLING_RETRY_MS), the
    // quarantined one must still be leashed.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 1,
        &degrading,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a standing quarantine must not re-run the gate on the transient cadence"
    );

    // The Broken leash is the one that reopens it.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_BROKEN_RETRY_MS + 1,
        &degrading,
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// The OUTER scan gate itself: a second tick inside `ROLLING_SCAN_GAP_MS`
/// runs no candidate scan at all — without touching `next_scan_ms`, which is
/// how every other tick test opens the gate and why none of them could see
/// this one. Deleting the gate ran the full scan every tick, silently.
#[test]
fn claude_rolling_tick_scan_gap_holds_a_second_tick_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-gap"], &[]);
    write_rolling_sidecar("cl-gap", 60 * 60 * 1000); // dying, always due
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let counting = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Refreshed // clears any widening
    };
    super::claude_rolling_tick(&config, &pacing, now, &counting);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the gap, gate untouched: the scan must not run. Pinned on the
    // SCAN STAMP, not only the gate_fn count — the per-profile retry widening
    // can absorb a deleted gate's extra scans and leave the count intact,
    // which is exactly how the deletion stayed invisible to every earlier
    // test in this family.
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_SCAN_GAP_MS - 1,
        &counting,
    );
    assert_eq!(
        pacing.lock().unwrap().next_scan_ms,
        now + super::ROLLING_SCAN_GAP_MS,
        "a tick inside the gap must return before touching the scan stamp"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a tick inside the scan gap runs no candidate scan"
    );

    // Past the gap it runs again, the gate still untouched. (The per-profile
    // retry widening is cleared first — a stub gate never advances the
    // sidecar, and this test pins the OUTER gate, not the widening the
    // still-due tests already own.)
    pacing.lock().unwrap().retry_after_ms.clear();
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_SCAN_GAP_MS + 1,
        &counting,
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// Wall-clock stamps get CLAMPED against a backwards NTP step: a scan stamp
/// or retry leash further out than its own maximum can only mean the clock
/// moved back under it, and comparing alone would suppress the scan for the
/// whole step while the sidecar it renews runs its clock down.
#[test]
fn claude_rolling_tick_survives_a_backwards_clock_step() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-ntp"], &[]);
    write_rolling_sidecar("cl-ntp", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    // What a backwards step leaves behind: stamps hours in the "future".
    pacing.lock().unwrap().next_scan_ms = now + 40 * super::ROLLING_SCAN_GAP_MS;
    pacing.lock().unwrap().retry_after_ms.insert(
        "cl-ntp".to_string(),
        super::RetryHold {
            not_before: now + 10 * super::ROLLING_BROKEN_RETRY_MS,
            // The real fingerprint: the fixture's files do not change during
            // the test, so the CLOCK clamp must be what releases this hold —
            // a watch that released it early would fail the tick-2 assert.
            watched: Some(crate::claude::credential_fingerprint(
                &crate::profile::ProfileName::from("cl-ntp"),
            )),
        },
    );

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let counting = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Refreshed
    };
    // Tick 1 clamps the scan stamp back into range (and runs nothing).
    super::claude_rolling_tick(&config, &pacing, now, &counting);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    // Tick 2, one gap later: the scan runs and clamps the retry leash to at
    // most one Broken leash from NOW — the profile is still leashed here.
    let t2 = now + super::ROLLING_SCAN_GAP_MS + 1;
    super::claude_rolling_tick(&config, &pacing, t2, &counting);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    // Tick 3, one clamped leash later: the profile runs. Unclamped, the
    // ten-leash stamp would have held it for most of a day longer.
    super::claude_rolling_tick(
        &config,
        &pacing,
        t2 + super::ROLLING_BROKEN_RETRY_MS + 1,
        &counting,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a backwards clock step delays the scan by at most its own bounds"
    );
}

/// A Broken verdict takes the LONG leash: without it, a profile whose chain
/// is terminally dead re-runs the whole gate — `RotationGuard` acquisition
/// included — every scan, forever, for a condition only a re-login changes.
#[test]
fn claude_rolling_tick_broken_verdict_takes_the_long_leash() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-dead"], &[]);
    write_rolling_sidecar("cl-dead", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let broken = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Broken
    };
    super::claude_rolling_tick(&config, &pacing, now, &broken);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Where a transient would retry, Broken must still be leashed.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(&config, &pacing, now + super::ROLLING_RETRY_MS + 1, &broken);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a dead chain is not retried on the transient cadence"
    );
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_BROKEN_RETRY_MS + 1,
        &broken,
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A Transient whose cause only a re-login clears takes the SAME long leash
/// as Broken: an unrecorded grant paced on the 15-minute cadence re-logs the
/// identical refusal ~24 times per bearer lifetime with no retry ever able to
/// succeed. Genuinely transient causes must keep the short cadence — that
/// asymmetry is the test.
#[test]
fn claude_rolling_tick_relogin_transients_take_the_long_leash() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-grant"], &[]);
    write_rolling_sidecar("cl-grant", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let unrecorded = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Transient(crate::format::Transient::new(
            crate::format::Cause::RollingGrantUnrecorded("cl-grant".to_string()),
            crate::format::Retry::Stated,
        ))
    };
    super::claude_rolling_tick(&config, &pacing, now, &unrecorded);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Where an ordinary transient would retry, the re-login refusal is still
    // leashed…
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 1,
        &unrecorded,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an unrecorded grant is not retried on the transient cadence"
    );
    // …and an ordinary transient keeps the short cadence on the same code
    // path, so the long leash demonstrably keys off the CAUSE.
    let ordinary = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Transient(crate::format::Transient::new(
            crate::format::Cause::Endpoint("could not reach anthropic"),
            crate::format::Retry::Wait,
        ))
    };
    pacing.lock().unwrap().next_scan_ms = 0;
    pacing.lock().unwrap().retry_after_ms.clear();
    super::claude_rolling_tick(&config, &pacing, now, &ordinary);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 1,
        &ordinary,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "a genuinely transient cause keeps the short cadence"
    );
}

/// The long leash's exit is the OPERATOR'S FIX, not the clock: a re-login
/// writes `credentials.json` and stamps nothing scheduler-side, so a hold that
/// only time releases would sit on the prescribed recovery for up to six
/// hours. The hold watches the profile's credential files; a change releases
/// it on the next scan — and unchanged files demonstrably do NOT release it,
/// or the leash would be no leash at all.
#[test]
fn claude_rolling_tick_relogin_hold_releases_on_a_credential_write() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-fix"], &[]);
    write_rolling_sidecar("cl-fix", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let unrecorded = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Transient(crate::format::Transient::new(
            crate::format::Cause::RollingGrantUnrecorded("cl-fix".to_string()),
            crate::format::Retry::Stated,
        ))
    };
    super::claude_rolling_tick(&config, &pacing, now, &unrecorded);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the leash with NOTHING changed on disk: still held.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 1,
        &unrecorded,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "unchanged credentials do not release the hold"
    );

    // The operator does exactly what the cause prescribed: a re-login, which
    // lands as a `credentials.json` write and nothing else.
    let dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-fix")).expect("dir");
    std::fs::write(
        dir.join("credentials.json"),
        serde_json::to_vec(&crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-fresh-login".to_string(),
                refresh_token: Some("rt-fresh-login".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("write fresh login");

    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 2,
        &unrecorded,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the credential write releases the hold on the next scan, hours early"
    );
}

/// The due predicate reads a MIS-FILL as due now: the content is the defect
/// (its clock is irrelevant), switches refuse to install it, and this leg is
/// the only one a running daemon has that can repair it. Without this, a
/// mis-filled sidecar beside a healthy preserved mint sat unrepaired forever
/// on any profile nobody switched to.
#[test]
fn claude_rolling_tick_reaches_the_gate_for_a_misfilled_sidecar() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-mf"], &[]);
    let dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-mf")).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A rotating pair in the sidecar, expiry comfortably OUTSIDE the re-stamp
    // horizon: only the content classification can make this due.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-misfill".to_string(),
                refresh_token: Some("rt-misfill".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("write misfill");

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    super::claude_rolling_tick(&config, &pacing, crate::usage::now_ms(), &|_| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Ready
    });
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a mis-filled sidecar reaches the gate — the repair leg actually fires"
    );
}

/// The backwards-clock clamp reads the hold's KIND off `watched`: a stretched
/// SHORT hold clamps back to the 15-minute cadence, never to the six-hour
/// leash — an unwatched six-hour hold would have no exit but the clock, the
/// exact stall the watch exists to remove, handed to a profile that never
/// earned the long leash.
#[test]
fn claude_rolling_tick_clamps_a_short_hold_to_its_own_leash() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-clk"], &[]);
    write_rolling_sidecar("cl-clk", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();
    // What a backwards step leaves behind on a SHORT (unwatched) hold.
    pacing.lock().unwrap().retry_after_ms.insert(
        "cl-clk".to_string(),
        super::RetryHold {
            not_before: now + 10 * super::ROLLING_BROKEN_RETRY_MS,
            watched: None,
        },
    );

    let calls = std::sync::atomic::AtomicUsize::new(0);
    let counting = |_: &crate::profile::ProfileName| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Refreshed
    };
    // Tick 1: the clamp pulls the stamp back to the SHORT cadence; still held.
    super::claude_rolling_tick(&config, &pacing, now, &counting);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    // Tick 2, one short cadence later: the profile runs. Clamped to the long
    // leash instead, this unwatched hold would sit for six hours with no
    // credential-change exit.
    pacing.lock().unwrap().next_scan_ms = 0;
    super::claude_rolling_tick(
        &config,
        &pacing,
        now + super::ROLLING_RETRY_MS + 1,
        &counting,
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a short hold clamps to its own leash, not the long one"
    );
}

/// Names that leave the candidate set — profile deleted, disabled, or the
/// flag turned off — take their retry stamps with them. Without the sweep the
/// map grows monotonically over the daemon's lifetime, and a re-created
/// profile of the same name inherits a stale leash it never earned.
#[test]
fn claude_rolling_tick_drops_retry_state_for_departed_profiles() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = rolling_profile_config(&["cl-alive"], &[]);
    write_rolling_sidecar("cl-alive", 60 * 60 * 1000);
    let pacing = crate::lockorder::RankedMutex::new(super::ClaudeRollingPacing::default());
    let now = crate::usage::now_ms();
    pacing.lock().unwrap().retry_after_ms.insert(
        "cl-deleted".to_string(),
        super::RetryHold {
            not_before: now + super::ROLLING_BROKEN_RETRY_MS,
            watched: None,
        },
    );
    super::claude_rolling_tick(&config, &pacing, now, &|_| crate::oauth::AuthGate::Ready);
    let p = pacing.lock().unwrap();
    assert!(
        !p.retry_after_ms.contains_key("cl-deleted"),
        "a departed profile's stamp is swept on the next scan"
    );
}

/// `elect_auto_start_queue`: the tick-level half of the interleaved auto-start queue
/// (`usage::auto_start_queue`). Its whole job is to narrow the permissive `may_open_window`
/// that `collect_tokens` sets down to at most ONE member per tick — the
/// serialisation that stops `fetch_oauth_due_with`'s per-profile workers from
/// each opening a window in the same tick.
///
/// Also pins the two sizing rules the gap depends on: queue size comes from the
/// SNAPSHOT (every participating member) rather than from the due list (only
/// this tick's cadence slots), and a member that cannot kick at all is excluded
/// so the queue does not reserve a slot for a corpse.
#[test]
fn auto_start_queue_election_picks_one_member_and_holds_the_rest() {
    use crate::profile::{AppConfig, AppState};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let _home = crate::testutil::HomeSandbox::new();

    let now = crate::usage::now_epoch_secs();
    let warming = |name: &str| {
        let mut e = token(name);
        e.auto_start = true;
        e
    };
    // Queue MEMBERSHIP comes from the config (`usage::auto_start_queue::auto_start_queue_members`, the
    // rule the `status.json` feed and the TUI's chips read too); the snapshot
    // below is the work-list the election narrows.
    let opted_in = |name: &str| {
        let mut p = oauth_profile_disabled(name, false);
        p.auto_start = true;
        p
    };

    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            fallback_chain: vec!["a".into(), "b".into(), "c".into()],
            // Opt in: the queue is default-off, and this test is the queue.
            auto_start_queue: true,
            ..Default::default()
        },
        profiles: vec![opted_in("a"), opted_in("b"), opted_in("c")],
    }));

    // All three have fetched, all three have lapsed windows: every one wants a
    // kick, so only the queue can be what holds two of them back.
    let lapsed = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(now - 60)),
        }),
        ..Default::default()
    };
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), lapsed.clone()),
        ("b".to_string(), lapsed.clone()),
        ("c".to_string(), lapsed.clone()),
    ])));

    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![])),
        store,
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    let snapshot = vec![warming("a"), warming("b"), warming("c")];

    // Cold queue, nothing persisted: the chain head opens, the other two hold.
    let mut due = snapshot.clone();
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    let elected: Vec<&str> = due
        .iter()
        .filter(|e| e.may_open_window)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        elected,
        vec!["a"],
        "exactly one member opens per tick, and it is the chain head"
    );

    // Inside the gap (a member opened a moment ago): NOBODY opens this tick.
    // This is the spacing itself — without it all three reopen together and the
    // whole feature is a no-op.
    state.auto_start_queue.lock().unwrap().last_open_at = Some(now - 60);
    let mut due = snapshot.clone();
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    assert!(
        due.iter().all(|e| !e.may_open_window),
        "inside the gap no member may open a window"
    );

    // One gap later the queue is due again. N=3 → 5h/3 less the tick tolerance.
    let gap = crate::usage::queue_gap_secs(3, REFRESH_INTERVAL_MS);
    state.auto_start_queue.lock().unwrap().last_open_at = Some(now - gap);
    let mut due = snapshot.clone();
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    assert_eq!(
        due.iter().filter(|e| e.may_open_window).count(),
        1,
        "once the gap has passed exactly one member opens again"
    );

    // Queue size is read off the whole QUEUE, not the due list: with only one
    // member due this tick the gap must still be 5h/3, not 5h/1. Anchored just
    // PAST the 3-member gap and asserted in the ELECT direction: the
    // right-sized gap elects `c`, while a gap wrongly sized off the 1-member
    // due list (5h less tolerance) would still hold. The hold direction could
    // not catch that mutant — `due` is a subset of the queue, so its gap is
    // always the larger one.
    state.auto_start_queue.lock().unwrap().last_open_at = Some(now - gap - 60);
    let mut only_c = vec![warming("c")];
    super::elect_auto_start_queue(&state, &mut only_c, REFRESH_INTERVAL_MS, now);
    assert!(
        only_c[0].may_open_window,
        "the gap is sized from the whole queue, so just past 5h/3 the lone due member opens"
    );

    // A member that cannot kick is not a queue member: quarantining the head
    // shrinks N to 2, which widens the gap, and hands the slot to `b`. The
    // quarantined profile itself is left UNSTAMPED — outside the queue the
    // election neither gates nor grants, so it keeps the permissive flag
    // `collect_tokens` set and keeps retrying on its own `kick_retry_due`
    // ladder. Stamping it false would deny it the lapsed leg on every tick,
    // permanently: only a landed kick clears a block.
    if let Ok(mut cfg) = state.config.lock() {
        cfg.set_auth_broken(&"a".into(), true);
    }
    state.auto_start_queue.lock().unwrap().last_open_at = None;
    let mut due = snapshot.clone();
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    let may_open: Vec<&str> = due
        .iter()
        .filter(|e| e.may_open_window)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        may_open,
        vec!["a", "b"],
        "`b` is elected; auth-broken `a` holds no slot but keeps its \
         permissive out-of-queue flag"
    );
    assert!(
        !due.iter().any(|e| e.name == "c" && e.may_open_window),
        "the unelected MEMBER is the one the election holds"
    );

    // Back to a whole 3-member queue for the two history-derived legs.
    if let Ok(mut cfg) = state.config.lock() {
        cfg.set_auth_broken(&"a".into(), false);
    }
    // An open NOBODY here fired: `c`'s window was opened out of band ten
    // minutes ago (a real Claude Code session on that account), and the only
    // record of it is the history series the poller writes. Seeded through the
    // real writer so the bridge lines are present.
    let reading = |reset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(reset)),
        }),
        ..Default::default()
    };
    let out_of_band = now - 600;
    let boundary = out_of_band + 5 * 3600;
    let mut prev: Option<UsageInfo> = None;
    for (ts, reset) in [
        (now - 180, boundary),
        (now - 90, boundary - 1),
        (now, boundary),
    ] {
        let next = reading(reset);
        crate::profile::append_usage_sample_at(&"c".into(), prev.as_ref(), &next, ts as u64 * 1000);
        prev = Some(next);
    }

    // Nothing lapsed: every window is live, so no member wants one opened. The
    // election answers without touching the anchor — an empty anchor stays
    // empty even though the series above would have filled it. That skip is
    // what keeps the replay off the majority of ticks (all windows live is the
    // long middle of every cycle) while leaving the gate's answer identical:
    // an election no one can win stamps every member shut regardless.
    let live = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 0.0,
            resets_at: Some(epoch_secs_to_iso(now + 3600)),
        }),
        ..Default::default()
    };
    if let Ok(mut m) = state.store.lock() {
        for name in ["a", "b", "c"] {
            m.insert(name.to_string(), live.clone());
        }
    }
    state.auto_start_queue.lock().unwrap().last_open_at = None;
    let mut due = snapshot.clone();
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    assert!(
        due.iter().all(|e| !e.may_open_window),
        "with nothing lapsed no member is elected"
    );
    assert_eq!(
        crate::usage::queue_anchor_cached(&state.auto_start_queue),
        None,
        "and the anchor is never derived on a tick that could not elect anyone"
    );

    // Now they lapse, with our own anchor a full gap stale — the pre-fix state
    // that let the next member kick seconds after `c`'s out-of-band open and
    // re-collapse the two windows. The series is the only thing that knows.
    if let Ok(mut m) = state.store.lock() {
        for name in ["a", "b", "c"] {
            m.insert(name.to_string(), lapsed.clone());
        }
    }
    state.auto_start_queue.lock().unwrap().last_open_at = Some(now - gap - 60);
    let mut due = snapshot.clone();
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    assert!(
        due.iter().all(|e| !e.may_open_window),
        "an out-of-band open gates the queue exactly as one of our own kicks does"
    );
    assert_eq!(
        crate::usage::queue_anchor_cached(&state.auto_start_queue),
        Some(out_of_band),
        "and it becomes the anchor the next gap is measured from"
    );
}

/// The toggle is a real off switch: with `auto_start_queue` false every entry
/// keeps the permissive `may_open_window` `collect_tokens` set, which is exactly the
/// pre-queue behaviour (every lapsed window reopens on its own tick).
#[test]
fn auto_start_queue_election_is_a_no_op_when_the_toggle_is_off() {
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let _home = crate::testutil::HomeSandbox::new();

    let now = crate::usage::now_epoch_secs();
    // Both accounts opted in and eligible: with the toggle ON they would be a
    // real 2-member queue, so an empty `may_open_window` set below could only come
    // from the toggle itself.
    let opted_in = |name: &str| {
        let mut p = oauth_profile_disabled(name, false);
        p.auto_start = true;
        p
    };
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            auto_start_queue: false,
            ..Default::default()
        },
        profiles: vec![opted_in("a"), opted_in("b")],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
        last_history_prune: AtomicU64::new(crate::usage::now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
    };

    let mut a = token("a");
    a.auto_start = true;
    let mut b = token("b");
    b.auto_start = true;
    let mut due = vec![a, b];
    // Anchor pinned to now: with the queue ON this would hold everyone.
    state.auto_start_queue.lock().unwrap().last_open_at = Some(now);
    super::elect_auto_start_queue(&state, &mut due, REFRESH_INTERVAL_MS, now);
    assert!(
        due.iter().all(|e| e.may_open_window),
        "with the toggle off the queue never narrows anything"
    );
}
