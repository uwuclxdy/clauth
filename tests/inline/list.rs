#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `clauth list` table renderer (`render_table`): hide/reveal of disabled
//! profiles, the active marker, and exact column layout. Driven over the real
//! `build_status` body under a `HomeSandbox`, the same data path
//! `clauth status --json` reads, so a drift in either surface reds here.

use super::*;

use crate::profile::{AppState, ClaudeCredentials, OAuthToken, Profile};
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::testutil::HomeSandbox;
use crate::usage::{PlanInfo, PlanTier, UsageInfo, UsageWindow};

fn oauth(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
        }),
    });
    p
}

/// Warm `name`'s OAuth usage cache: a `Max 5x` plan and fixed 5h/7d utilization
/// so the rounding and the plan label are pinned, not incidental.
fn warm_usage(name: &str, five_h: f64, seven_d: f64) {
    // The cache write is gated on the on-disk record; the row this warms is the
    // test's pin, so the name has to exist in the record for the write to land.
    crate::testutil::register_names(&[name]);
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(5)),
                subscription_status: None,
                codex_plan: None,
            }),
            five_hour: Some(UsageWindow {
                utilization: five_h,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: seven_d,
                resets_at: None,
            }),
            ..Default::default()
        },
    );
}

const HEADER: &str = "  PROFILE  PLAN       5H     7D  ENDPOINT";
// 42.4 → 42.4%, 17.6 → 17.6%: format_pct drops only trailing `.0`.
const WORK_ROW: &str = "* work     Max 5x  42.4%  17.6%  -";

#[test]
fn list_table_hides_disabled_by_default_and_marks_the_active_profile() {
    let _home = HomeSandbox::new();
    let mut off = oauth("off");
    off.disabled = true;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work"), off],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);

    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [HEADER, WORK_ROW],
        "only the active profile is shown"
    );
    assert!(
        !table.contains("off"),
        "a disabled profile must not appear without --all/--disabled"
    );
}

#[test]
fn list_table_reveals_disabled_with_a_trailing_marker_when_included() {
    let _home = HomeSandbox::new();
    let mut off = oauth("off");
    off.disabled = true;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work"), off],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, true);
    let table = render_table(&config, &entries);

    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            HEADER,
            WORK_ROW,
            "  off      Max         -      -  - (disabled)",
        ],
        "the disabled row keeps its columns aligned and carries the (disabled) marker"
    );
}

/// Warm `name`'s cache as a CANCELED account: the org has already dropped to
/// `claude_free`, which is what makes the tier alone unable to carry the fact.
fn warm_canceled(name: &str) {
    crate::testutil::register_names(&[name]);
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        USAGE_CACHE_FILE,
        &UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Free,
                subscription_status: Some("canceled".to_string()),
                codex_plan: None,
            }),
            ..Default::default()
        },
    );
}

/// This table has no status column, so the trailing marker is the only place a
/// cancellation can appear. The PLAN column keeps the real tier — a canceled org
/// reads `Free`, which is indistinguishable from a genuine free account without
/// the marker.
#[test]
fn list_table_marks_a_canceled_account_and_keeps_its_real_tier() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work"), oauth("dead")],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);
    warm_canceled("dead");

    let table = render_table(
        &config,
        &build_profile_entries(&config, config.state.refresh_interval_ms, None, true),
    );
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            HEADER,
            WORK_ROW,
            "  dead     Free        -      -  - (canceled)",
        ],
        "the canceled row keeps its tier in PLAN and carries the marker"
    );
}

/// A healthy account carries no marker at all — the guard that the suffix is
/// driven by the cached status and not by merely having a cache.
#[test]
fn list_table_leaves_a_live_account_unmarked() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work")],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);

    let table = render_table(
        &config,
        &build_profile_entries(&config, config.state.refresh_interval_ms, None, true),
    );
    assert_eq!(table.lines().collect::<Vec<_>>(), [HEADER, WORK_ROW]);
    assert!(
        !table.contains('('),
        "a live account carries no state marker, got {table:?}"
    );
}

/// Both facts render. An operator usually disables an account BECAUSE it died,
/// so a `disabled` that masked `canceled` would hide the reason for the state it
/// is reporting — the same erasure the Fallback tab's stacked pills prevent.
#[test]
fn list_table_stacks_disabled_and_canceled_rather_than_letting_one_win() {
    let _home = HomeSandbox::new();
    let mut dead = oauth("dead");
    dead.disabled = true;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work"), dead],
    };
    config.state.active_profile = Some("work".into());
    warm_usage("work", 42.4, 17.6);
    warm_canceled("dead");

    let table = render_table(
        &config,
        &build_profile_entries(&config, config.state.refresh_interval_ms, None, true),
    );
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            HEADER,
            WORK_ROW,
            "  dead     Free        -      -  - (disabled, canceled)",
        ],
        "neither state may hide the other"
    );
}

#[test]
fn list_table_shows_provider_as_plan_and_the_base_url_endpoint_for_a_third_party() {
    let _home = HomeSandbox::new();
    let mut zai = Profile::new(
        "z.ai".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("sk-test".to_string()),
    );
    zai.provider = crate::providers::Provider::from_base_url("https://api.z.ai/api/anthropic");
    assert!(
        zai.is_third_party(),
        "fixture must be a third-party account"
    );
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);

    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            "  PROFILE  PLAN  5H  7D  ENDPOINT",
            "  z.ai     Z.ai   -   -  https://api.z.ai/api/anthropic",
        ],
        "a third-party account shows its provider as the plan and its base url as the endpoint"
    );
}

#[test]
fn list_table_reports_no_accounts_when_empty() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, true);
    assert_eq!(
        render_table(&config, &entries),
        "no accounts yet. add one with `clauth login <name>`.\n"
    );
}

/// The table shows window percentages with no freshness column, so a dead
/// console session behind a warm cache rendered as ordinary live numbers. The
/// state suffix — already the place for facts the columns can't hold — is where
/// that has to surface.
#[test]
fn a_dead_credential_is_named_in_the_state_suffix() {
    let _home = crate::testutil::HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let mut p = crate::profile::Profile::new("qwen".to_string(), Some(base.to_string()), None);
    p.provider = crate::providers::Provider::from_base_url(base);
    p.console = Some(crate::profile::ConsoleCredential {
        token: "dead".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    let config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["qwen".into()],
            ..crate::profile::AppState::default()
        },
        profiles: vec![p],
    };
    let fp = crate::usage::profile_credential_fingerprint(&config.profiles[0]).unwrap();
    crate::testutil::register_names(&["qwen"]);
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("qwen"), fp);

    let entries = crate::daemon::build_profile_entries(&config, 300_000, None, false);
    let table = render_table(&config, &entries);
    assert!(
        table.contains("login expired"),
        "the table must name a credential that will never self-heal, got:\n{table}"
    );
}

/// The same dead-credential state has two causes wanting opposite actions, and
/// the api-key-only account is the common one: its key works for inference and
/// authenticates nothing on the usage gateway, so it lands here having never
/// stored a session. "expired" would send that operator looking for something to
/// renew.
#[test]
fn a_profile_that_never_stored_a_session_is_told_it_needs_one() {
    let _home = crate::testutil::HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let mut p = crate::profile::Profile::new(
        "qwen".to_string(),
        Some(base.to_string()),
        Some("sk-sp-a-perfectly-good-inference-key".to_string()),
    );
    p.provider = crate::providers::Provider::from_base_url(base);
    assert!(p.console.is_none(), "the account has only its api key");
    let config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["qwen".into()],
            ..crate::profile::AppState::default()
        },
        profiles: vec![p],
    };
    let fp = crate::usage::profile_credential_fingerprint(&config.profiles[0]).unwrap();
    crate::testutil::register_names(&["qwen"]);
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("qwen"), fp);

    let entries = crate::daemon::build_profile_entries(&config, 300_000, None, false);
    let table = render_table(&config, &entries);
    assert!(
        table.contains("login needed"),
        "an account that never had a session is not expired, got:\n{table}"
    );
    assert!(
        !table.contains("login expired"),
        "nothing lapsed here, so nothing may say it did, got:\n{table}"
    );
}

/// A non-Alibaba profile has no session to lapse, so its `AuthExpired` can only
/// mean the api key was rejected — the suffix must say so, or the operator goes
/// hunting for a login that does not exist.
#[test]
fn a_dead_api_key_is_told_the_key_was_rejected() {
    let _home = crate::testutil::HomeSandbox::new();
    let base = "https://api.deepseek.com/anthropic";
    let mut p = crate::profile::Profile::new("deepseek".to_string(), Some(base.to_string()), None);
    p.provider = crate::providers::Provider::from_base_url(base);
    p.api_key = Some("sk-revoked".to_string());
    let config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["deepseek".into()],
            ..crate::profile::AppState::default()
        },
        profiles: vec![p],
    };
    let fp = crate::usage::profile_credential_fingerprint(&config.profiles[0]).unwrap();
    crate::testutil::register_names(&["deepseek"]);
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("deepseek"), fp);

    let entries = crate::daemon::build_profile_entries(&config, 300_000, None, false);
    let table = render_table(&config, &entries);
    assert!(
        table.contains("key rejected"),
        "a dead api key is not a login problem, got:\n{table}"
    );
    assert!(
        !table.contains("login"),
        "no session exists to name, got:\n{table}"
    );
}
