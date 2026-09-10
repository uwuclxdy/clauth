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
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    p
}

/// Warm `name`'s OAuth usage cache: a `Max 5x` plan and fixed 5h/7d utilization
/// so the rounding and the plan label are pinned, not incidental.
/// A cache a live fetch just wrote. Stamped, because an undated body is the
/// one shape the age contract reads as stale, and these rows pin the healthy
/// table.
fn warm_usage(name: &str, five_h: f64, seven_d: f64) {
    warm_usage_at(name, five_h, seven_d, Some(crate::usage::now_ms()));
}

fn warm_usage_at(name: &str, five_h: f64, seven_d: f64, fetched_at: Option<u64>) {
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
            }),
            five_hour: Some(UsageWindow {
                utilization: five_h,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: seven_d,
                resets_at: None,
            }),
            fetched_at,
            ..Default::default()
        },
    );
}

const HEADER: &str = "  PROFILE  PLAN    5H USED  7D USED  ENDPOINT";

/// ISO-8601 UTC `now_secs + ahead_secs`, the shape `resets_at` carries.
fn future_iso(ahead_secs: i64) -> String {
    crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() + ahead_secs)
}
// 42.4 → 42.4%, 17.6 → 17.6%: format_pct drops only trailing `.0`.
const WORK_ROW: &str = "* work     Max 5x    42.4%    17.6%  -";

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
            "  off      Max           -        -  - (disabled)",
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
            "  dead     Free          -        -  - (canceled)",
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
            "  dead     Free          -        -  - (disabled, canceled)",
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
            "  PROFILE  PLAN  5H USED  7D USED  ENDPOINT",
            "  z.ai     Z.ai        -        -  https://api.z.ai/api/anthropic",
        ],
        "a third-party account shows its provider as the plan and its base url as the endpoint"
    );
}

/// A third-party profile with no inference auth source reads as viable —
/// indistinguishable from a keyed one — unless the row names the state. The
/// word is the MCP roster's own `keyless` flag, so the two surfaces cannot
/// spell one state two ways.
#[test]
fn list_table_marks_a_keyless_third_party_profile() {
    let _home = HomeSandbox::new();
    let url = "https://api.z.ai/api/anthropic";
    let zai = Profile::new("z.ai".to_string(), Some(url.to_string()), None);
    assert!(
        zai.is_third_party(),
        "fixture must be a third-party account"
    );
    assert!(
        !crate::claude::has_inference_auth(&zai),
        "fixture must have no inference auth source"
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
            "  PROFILE  PLAN  5H USED  7D USED  ENDPOINT",
            "  z.ai     Z.ai        -        -  https://api.z.ai/api/anthropic (keyless)",
        ],
        "a keyless third-party row names the state"
    );
}

/// An env token is an inference auth source too: the predicate is the delegate
/// guard's own (`has_inference_auth`), not `api_key.is_some()`, so an
/// env-keyed third-party row must read exactly like an api-keyed one.
#[test]
fn list_table_leaves_an_env_keyed_third_party_profile_unmarked() {
    let _home = HomeSandbox::new();
    let url = "https://api.z.ai/api/anthropic";
    let mut zai = Profile::new("z.ai".to_string(), Some(url.to_string()), None);
    zai.env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        "sk-env-token".to_string(),
    );
    assert!(
        crate::claude::has_inference_auth(&zai),
        "fixture must have an inference auth source via env"
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
            "  PROFILE  PLAN  5H USED  7D USED  ENDPOINT",
            "  z.ai     Z.ai        -        -  https://api.z.ai/api/anthropic",
        ],
        "an env token keys the row, so it carries no marker"
    );
}

/// A third-party account's 5h/7d columns render the PROVIDER's own headroom —
/// its cached usage bars — rather than dashes (owner ruling 2026-09-09 row 3:
/// the defect was the empty columns, not the marker). The bar arms mirror the
/// roster rank's fall-through: a live bar's `pct`, falling back to the first
/// funded wallet when no label-matched live bar exists. Lapsed bars and spent
/// wallets keep reading as dashes, the same missing-data call every other
/// surface makes. The `(stale)` marker itself is pinned by its own test below.
#[test]
fn list_table_renders_a_third_party_rows_own_headroom() {
    let _home = HomeSandbox::new();
    let url = "https://api.z.ai/api/anthropic";
    let mut zai = Profile::new("z.ai".to_string(), Some(url.to_string()), Some("k".into()));
    zai.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };

    crate::testutil::register_names(&["z.ai"]);
    // The bar shape off a real capture, re-anchored to stay live, written
    // through the production cache writer.
    crate::testutil::write_captured_third_party_cache(
        "z.ai",
        crate::testutil::THIRD_PARTY_BARS_CACHE_BYTES,
    );

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            "  PROFILE  PLAN  5H USED  7D USED  ENDPOINT",
            "  z.ai     Z.ai    12.5%      48%  https://api.z.ai/api/anthropic",
        ],
        "a third-party row shows its own bars in the 5h/7d columns"
    );
}

/// A live bar under a label the columns do not spell (`time limit`, the
/// generic scanner's provider-authored label) neither fills a column nor
/// suppresses the wallet fallback — the same fall-through the roster's rank
/// uses, so the two surfaces cannot disagree about which figure speaks for
/// the account.
#[test]
fn list_table_falls_through_to_the_wallet_over_a_non_canonical_bar_label() {
    let _home = HomeSandbox::new();
    let url = "https://api.z.ai/api/anthropic";
    let mut zai = Profile::new("z.ai".to_string(), Some(url.to_string()), Some("k".into()));
    zai.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };

    crate::testutil::register_names(&["z.ai"]);
    // The captured bars with live (re-anchored) stamps, every label renamed to
    // the generic scanner's shape: LIVE bars exist, none matches `5h`/`7d`, and
    // the wallet row is what the rank falls through to.
    let mut stats: crate::providers::ThirdPartyStats =
        serde_json::from_slice(&crate::testutil::reanchored_bars_cache_bytes(
            crate::testutil::THIRD_PARTY_BARS_CACHE_BYTES,
        ))
        .unwrap();
    for bar in &mut stats.bars {
        bar.label = "time limit".to_string();
    }
    stats.rows.push(crate::providers::StatRow {
        label: "total".to_string(),
        value: "31.45 CNY".to_string(),
        kind: crate::providers::StatRowKind::Body,
    });
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("z.ai"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);
    assert!(
        table.contains("31.45 CNY"),
        "a live but non-canonically-labeled bar does not suppress the wallet: {table}"
    );
    assert!(
        !table.contains("12.5%"),
        "a non-canonical bar never fills a column: {table}"
    );
}

/// The wallet fallback: a scalar provider (DeepSeek) has no bars, so the first
/// FUNDED wallet's spendable balance stands in for the headroom the columns
/// exist to show — one figure, in the 5h column, same as the MCP headline.
#[test]
fn list_table_falls_back_to_the_first_funded_wallet_when_no_bar_exists() {
    let _home = HomeSandbox::new();
    let url = "https://api.deepseek.com/anthropic";
    let mut ds = Profile::new(
        "deepseek".to_string(),
        Some(url.to_string()),
        Some("k".into()),
    );
    ds.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![ds],
    };

    crate::testutil::register_names(&["deepseek"]);
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("deepseek"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, crate::testutil::DEEPSEEK_CACHE_BYTES).unwrap();

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            "  PROFILE   PLAN        5H USED  7D USED  ENDPOINT",
            "  deepseek  DeepSeek  31.45 CNY        -  https://api.deepseek.com/anthropic",
        ],
        "a scalar provider's funded wallet stands in for the 5h column"
    );
}

/// The liveness filter inside the column lookup: the captured bars' own
/// `resets_at` stamps (2026-08) are past, so writing the fixture VERBATIM —
/// no re-anchoring — pins that a lapsed bar renders dashes rather than the
/// previous window's last reading (#74's drop, held on the list surface too).
#[test]
fn list_table_dashes_a_lapsed_bars_last_reading() {
    let _home = HomeSandbox::new();
    let url = "https://api.z.ai/api/anthropic";
    let mut zai = Profile::new("z.ai".to_string(), Some(url.to_string()), Some("k".into()));
    zai.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };

    crate::testutil::register_names(&["z.ai"]);
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("z.ai"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, crate::testutil::THIRD_PARTY_BARS_CACHE_BYTES).unwrap();

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines,
        [
            "  PROFILE  PLAN  5H USED  7D USED  ENDPOINT",
            "  z.ai     Z.ai        -        -  https://api.z.ai/api/anthropic",
        ],
        "a lapsed bar's last reading is no reading: the columns stay dashes"
    );
}

/// The wallet fallback takes the first FUNDED wallet in ROW order (owner
/// ruling 2026-08-28): a two-wallet cache listing the empty one first must
/// render the funded figure, not the zero it would show on a `.last()` or
/// amount-sorting read.
#[test]
fn list_table_ranks_the_first_funded_wallet_not_the_empty_first_listed() {
    let _home = HomeSandbox::new();
    let url = "https://api.deepseek.com/anthropic";
    let mut ds = Profile::new(
        "deepseek".to_string(),
        Some(url.to_string()),
        Some("k".into()),
    );
    ds.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![ds],
    };

    crate::testutil::register_names(&["deepseek"]);
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("deepseek"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, crate::testutil::CAPTURED_TWO_WALLET_DS_CACHE).unwrap();

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);
    assert!(
        table.contains("498.18 CNY"),
        "the first funded wallet (row order) is the figure, got:\n{table}"
    );
    assert!(
        !table.contains("0.00 USD"),
        "the empty wallet listed first must not win, got:\n{table}"
    );
}

/// `x / y` used-and-total bars (z.ai's window shape) render the absolute
/// amounts, not the percentage: the percentage is derived, `used / total` is
/// the account's own reported figure.
#[test]
fn list_table_renders_a_bars_own_absolute_amounts_when_it_carries_them() {
    let _home = HomeSandbox::new();
    let url = "https://api.z.ai/api/anthropic";
    let mut zai = Profile::new("z.ai".to_string(), Some(url.to_string()), Some("k".into()));
    zai.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };

    crate::testutil::register_names(&["z.ai"]);
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("z.ai"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // The 5h bar loses its `resets_at` (unstamped bars stay, the missing-data
    // call every other surface makes) and gains `used`/`total`.
    std::fs::write(
        &path,
        crate::testutil::THIRD_PARTY_BARS_CACHE_BYTES
            .replace(
                "\"5h\",\"pct\":12.5,\"resets_at\":\"2026-08-15T12:00:00Z\"",
                "\"5h\",\"pct\":12.5,\"used\":50.0,\"total\":400.0",
            )
            .replace("2026-08-20T00:00:00Z", &future_iso(7 * 86400)),
    )
    .unwrap();

    let entries = build_profile_entries(&config, config.state.refresh_interval_ms, None, false);
    let table = render_table(&config, &entries);
    assert!(
        table.contains("50 / 400"),
        "the 5h column shows the bar's own absolute amounts, got:\n{table}"
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

/// The table has no freshness column, so a warm cache old enough to be
/// distrusted renders the `(stale)` suffix — the same cue `status --json`
/// publishes, surfaced where the table's numbers would otherwise read as live.
/// The third-party arm of that cue: a third-party row's marker rides the
/// provider cache's own mtime past the same threshold, and it must KEEP riding
/// once the columns carry figures (the marker qualifies the headroom the
/// columns now show, so the two land together).
#[test]
fn list_table_marks_a_stale_reading() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("work")],
    };
    config.state.active_profile = Some("work".into());
    let age_ms = crate::profile_json::stale_after_ms(config.state.refresh_interval_ms) + 60_000;
    warm_usage_at("work", 42.4, 17.6, Some(crate::usage::now_ms() - age_ms));

    let table = render_table(
        &config,
        &build_profile_entries(&config, config.state.refresh_interval_ms, None, true),
    );
    assert!(
        table.contains("(stale)"),
        "an old cache must render the stale cue, got:\n{table}"
    );

    let url = "https://api.z.ai/api/anthropic";
    let mut zai = Profile::new("zai".to_string(), Some(url.to_string()), Some("k".into()));
    zai.provider = crate::providers::Provider::from_base_url(url);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![zai],
    };
    crate::testutil::register_names(&["zai"]);
    // Re-anchored bar stamps (live) under a backdated mtime: the marker and
    // the figures must land together.
    crate::testutil::write_captured_third_party_cache(
        "zai",
        crate::testutil::THIRD_PARTY_BARS_CACHE_BYTES,
    );
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("zai"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    crate::testutil::set_mtime(
        &path,
        std::time::SystemTime::now() - std::time::Duration::from_millis(age_ms),
    );
    let table = render_table(
        &config,
        &build_profile_entries(&config, config.state.refresh_interval_ms, None, true),
    );
    assert!(
        table.contains("(stale)"),
        "a third-party row's stale marker rides its own cache age, got:\n{table}"
    );
    assert!(
        table.contains("48%") || table.contains("12.5%"),
        "and the columns still carry the (stale-marked) figures, got:\n{table}"
    );
}

/// `auth_status: "broken"` means the OAuth credential is dead and re-auth is the
/// action — the same `(login expired)` the fetch-status path renders, read off
/// the entry rather than re-derived from config.
#[test]
fn list_table_names_a_broken_oauth_credential() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth("dead")],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("dead"), true);

    let table = render_table(
        &config,
        &build_profile_entries(&config, config.state.refresh_interval_ms, None, true),
    );
    assert!(
        table.contains("login expired"),
        "a broken OAuth credential must read as login expired, got:\n{table}"
    );
}

/// A profile can be both `auth_status: "broken"` and `fetch_status:
/// "AuthExpired"` — a hybrid whose OAuth refresh was rejected and whose api
/// key was too. Both render per the stacked rule; the identical-label pair
/// (broken + lapsed console) collapses to one.
#[test]
fn list_table_stacks_both_dead_credentials_and_dedupes_the_identical_pair() {
    let _home = crate::testutil::HomeSandbox::new();
    let base = "https://api.deepseek.com/anthropic";
    let mut p = crate::profile::Profile::new(
        "hybrid".to_string(),
        Some(base.to_string()),
        Some("sk-rejected".to_string()),
    );
    p.provider = crate::providers::Provider::from_base_url(base);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "hybrid-access".to_string(),
            refresh_token: Some("hybrid-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let mut config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["hybrid".into()],
            ..crate::profile::AppState::default()
        },
        profiles: vec![p],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("hybrid"), true);
    let fp = crate::usage::profile_credential_fingerprint(&config.profiles[0]).unwrap();
    crate::testutil::register_names(&["hybrid"]);
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("hybrid"), fp);

    let table = render_table(
        &config,
        &build_profile_entries(&config, 300_000, None, false),
    );
    assert!(
        table.contains("login expired, key rejected"),
        "a broken OAuth pair over a rejected key renders both labels, got:\n{table}"
    );
    assert_eq!(
        table.matches("login expired").count(),
        1,
        "the broken label renders once, got:\n{table}"
    );
}

/// The identical-label pair collapses: a broken OAuth pair beside a lapsed
/// Alibaba console both read `login expired`, and the label says nothing twice.
#[test]
fn list_table_collapses_the_identical_login_expired_pair() {
    let _home = crate::testutil::HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let mut p = crate::profile::Profile::new("qwen".to_string(), Some(base.to_string()), None);
    p.provider = crate::providers::Provider::from_base_url(base);
    p.console = Some(crate::profile::ConsoleCredential {
        token: "dead".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    let mut config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["qwen".into()],
            ..crate::profile::AppState::default()
        },
        profiles: vec![p],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("qwen"), true);
    let fp = crate::usage::profile_credential_fingerprint(&config.profiles[0]).unwrap();
    crate::testutil::register_names(&["qwen"]);
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("qwen"), fp);

    let table = render_table(
        &config,
        &build_profile_entries(&config, 300_000, None, false),
    );
    assert_eq!(
        table.matches("login expired").count(),
        1,
        "one dead credential renders one login-expired cue, got:\n{table}"
    );
}
