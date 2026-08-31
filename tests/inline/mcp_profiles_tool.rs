#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Guard coverage for the MCP `profiles` tool's response shape.
//!
//! It is the largest single thing clauth puts in front of a model — 3,854 real
//! tokens across one operator's 27 profiles before this trim, against 955 for
//! the whole init block — and its own description tells the model to call it at
//! session start. So the two things keeping it small are worth pinning: the
//! `names` filter, and the fields that appear only when they carry news.
//!
//! The `scope: "session"` arm is the folded-in former `which` tool: it resolves
//! through the same `which::resolve_active` tiers the session itself resolves
//! by, renders its one row through the roster's own `profile_line`, and carries
//! `source`.

use super::*;

use crate::profile::{
    AppState, ClaudeCredentials, OAuthToken, Profile, save_app_state, save_profile,
};
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::testutil::{ConfigDirSandbox, HomeSandbox};
use crate::usage::{PlanInfo, PlanTier, UsageInfo};

/// Two profiles: one plain OAuth account, one third-party with an endpoint.
fn seed_two_profiles() {
    save_profile(&Profile::new("solo".to_string(), None, None)).expect("save solo");
    save_profile(&Profile::new(
        "vendor".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    ))
    .expect("save vendor");
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "vendor".into()],
        ..Default::default()
    })
    .expect("save state");
}

/// Drive the async tool on a current-thread runtime.
fn call_profiles(names: Option<Vec<&str>>, scope: Option<&str>) -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .profiles(Parameters(ProfilesArgs {
                names: names.map(|v| v.into_iter().map(str::to_string).collect()),
                scope: scope.map(str::to_string),
            }))
            .await
    })
    .expect("profiles returns a tool result, never a transport error")
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

/// The reply's prose lines, one per roster row.
fn lines(result: &CallToolResult) -> Vec<String> {
    first_text(result).lines().map(str::to_string).collect()
}

#[test]
fn names_filter_selects_one_profile_case_insensitively() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    assert_eq!(
        lines(&call_profiles(None, None)),
        vec![
            "- solo (global active) [anthropic]: usage unknown; tier unknown",
            "- vendor [DeepSeek, api.deepseek.com]: usage unknown; no api key",
        ],
        "fixture control: both profiles are visible unfiltered",
    );
    // Wrong case on purpose: the filter resolves through `canonical_name`, the
    // same guard `switch_profile` applies, so a model need not know the stored
    // casing.
    assert_eq!(
        lines(&call_profiles(Some(vec!["VENDOR"]), None)),
        vec!["- vendor [DeepSeek, api.deepseek.com]: usage unknown; no api key"],
    );
    // An empty list is the same ask as no list at all, never "nothing".
    assert_eq!(
        lines(&call_profiles(Some(Vec::new()), None)).len(),
        2,
        "an empty `names` list still answers with every profile",
    );
}

/// A name matching nothing fails loudly. Dropping it silently would answer with
/// a roster that reads exactly like "that profile no longer exists", and the
/// model would act on the wrong one of those two readings.
#[test]
fn an_unresolvable_name_is_refused_and_named() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    let result = call_profiles(Some(vec!["solo", "ghost"]), None);
    assert_eq!(result.is_error, Some(true));
    let text = first_text(&result);
    assert!(
        text.starts_with("error: "),
        "a refusal reads as one: {text}"
    );
    assert!(text.contains("ghost"), "the reason names the bad input");
    assert!(!text.contains("solo"), "and only the bad input: {text}");
    assert!(text.contains("names"), "and the fix: {text}");
}

/// A scope that is neither `all` nor `session` is refused by name: a typo must
/// not silently answer the wrong question, which for two scopes is half of
/// them.
#[test]
fn an_unrecognised_scope_is_refused_by_name() {
    let _home = HomeSandbox::new();
    let result = call_profiles(None, Some("sessions"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        first_text(&result),
        "error: unrecognized scope \"sessions\": accepted \"all\" and \"session\""
    );
}

/// The trim itself. `has_live_session` and `throughput` are absent unless they
/// say something, and the endpoint prints as a host. Emitted unconditionally
/// these were 39% of a 27-profile response, nearly all of it `false` and rows
/// carrying no warning at all.
#[test]
fn quiet_fields_are_absent_and_the_endpoint_prints_as_a_host() {
    let _home = HomeSandbox::new();
    seed_two_profiles();

    let text = first_text(&call_profiles(None, None));
    let mut rows = text.lines();

    let solo = rows.next().expect("solo line");
    assert!(
        solo.starts_with("- solo (global active) [anthropic]"),
        "{solo}"
    );
    assert!(
        !solo.contains("live session"),
        "no live session, so the flag must not appear",
    );
    assert!(
        !solo.contains("throughput"),
        "no degraded model, so the field must not appear",
    );
    assert!(
        !solo.contains("https://"),
        "the endpoint must print as a host, never in full",
    );

    let vendor = rows.next().expect("vendor line");
    // Host only: every profile of one provider repeats the same path, and the
    // cost model only ever asks whether the host is loopback or LAN.
    assert!(
        vendor.contains("[DeepSeek, api.deepseek.com]"),
        "the bracket carries the host: {vendor}",
    );

    // The fields a picker always needs stay spelled, `unknown` included, so
    // their absence never has to be guessed at.
    assert!(
        solo.contains("usage unknown") && solo.contains("tier unknown"),
        "a null window and a null tier read as unknown, never drop out: {solo}",
    );
    assert!(
        solo.contains("(global active)"),
        "the active marker is present"
    );
}

/// Three profiles spanning the auth states: an OAuth account, a keyed
/// third-party, and a keyless third-party. The keyless one is the state the
/// roster's `keyless` flag must separate from "balance not fetched yet".
fn seed_auth_states() {
    save_profile(&Profile::new("solo".to_string(), None, None)).expect("save solo");
    save_profile(&Profile::new(
        "keyed".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-keyed-123".to_string()),
    ))
    .expect("save keyed");
    save_profile(&Profile::new(
        "keyless".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    ))
    .expect("save keyless");
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "keyed".into(), "keyless".into()],
        ..Default::default()
    })
    .expect("save state");
}

/// `keyless` is the only-when-news signal a picker needs before a `delegate`:
/// true on a third-party profile with no inference auth source, absent (never
/// `false`) on a keyed third-party and on an OAuth profile.
#[test]
fn keyless_flag_names_only_the_keyless_third_party_profile() {
    let _home = HomeSandbox::new();
    seed_auth_states();

    let text = first_text(&call_profiles(None, None));
    assert!(
        !text.lines().next().unwrap().contains("no api key"),
        "an OAuth profile never carries the keyless clause",
    );
    assert!(
        !text.contains("keyed [DeepSeek, api.deepseek.com]: usage unknown; no"),
        "a keyed third-party profile must not carry the keyless clause",
    );
    assert!(
        text.contains("- keyless [DeepSeek, api.deepseek.com]: usage unknown; no api key"),
        "the keyless profile names its missing api key in words: {text}",
    );
}

/// The prose names the keyless profile in words and leaves the keyed and OAuth
/// lines exactly as they rendered before the field existed.
#[test]
fn prose_names_the_keyless_profile_and_leaves_the_others_unchanged() {
    let _home = HomeSandbox::new();
    seed_auth_states();

    let lines = lines(&call_profiles(None, None));

    assert_eq!(
        lines,
        vec![
            "- solo (global active) [anthropic]: usage unknown; tier unknown".to_string(),
            "- keyed [DeepSeek, api.deepseek.com]: usage unknown".to_string(),
            "- keyless [DeepSeek, api.deepseek.com]: usage unknown; no api key".to_string(),
        ],
        "three lines: the OAuth and keyed lines render as before, the keyless          one names its missing api key in words",
    );
}

/// Four accounts, one per state the picker has to see before it spends: a
/// plain OAuth control, an operator-disabled one, a quarantined one
/// (`AppState::auth_broken`), and one whose subscription was canceled.
fn seed_flag_states() {
    save_profile(&Profile::new("solo".to_string(), None, None)).expect("save solo");
    let mut off = Profile::new("off".to_string(), None, None);
    off.disabled = true;
    save_profile(&off).expect("save off");
    save_profile(&Profile::new("dead".to_string(), None, None)).expect("save dead");
    save_profile(&Profile::new("gone".to_string(), None, None)).expect("save gone");
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "off".into(), "dead".into(), "gone".into()],
        auth_broken: vec!["dead".into()],
        ..Default::default()
    })
    .expect("save state");
    // The org drops to `claude_free` when a subscription is canceled, so the
    // cached `/profile` plan is where cancellation is readable at all.
    write_profile_cache(
        &crate::profile::ProfileName::from("gone"),
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

/// The three account-state markers render adjacent, so a reader meets one
/// group, and `canceled` follows them as the informational marker it is —
/// clauth has no cancel gate. Two of the three refuse a delegate outright;
/// `login expired` refuses only where the expired login is what the account
/// authenticates with (`preflight_target`). Each is absent
/// (never `false`) on an account it does not describe, the rule `keyless`
/// already ships, and a row in none of the states is byte-unchanged.
#[test]
fn roster_flags_name_each_state_and_leave_a_clean_row_unchanged() {
    let _home = HomeSandbox::new();
    seed_flag_states();

    assert_eq!(
        lines(&call_profiles(None, None)),
        vec![
            "- solo (global active) [anthropic]: usage unknown; tier unknown".to_string(),
            "- off [anthropic]: usage unknown; tier unknown; disabled".to_string(),
            "- dead [anthropic]: usage unknown; tier unknown; login expired".to_string(),
            "- gone [anthropic, Free]: usage unknown; subscription canceled".to_string(),
        ],
        "one marker per row, and the unaffected row renders exactly as before",
    );
}

/// The marker group is contiguous: a profile in all three states spells them
/// in one run, ahead of the informational `canceled`, rather than scattering
/// them through the line.
#[test]
fn the_three_state_markers_render_as_one_group() {
    let line = render::profiles_prose(&serde_json::json!({
        "profiles": [{
            "name": "wreck",
            "active": false,
            "provider": "DeepSeek",
            "tier": null,
            "host": "api.deepseek.com",
            "windows": {"kind": "third_party"},
            "has_live_session": true,
            "disabled": true,
            "auth_broken": true,
            "keyless": true,
            "canceled": true,
        }]
    }));
    assert_eq!(
        line,
        "- wreck [DeepSeek, api.deepseek.com]: usage unknown; live session; disabled; login expired; no api key; subscription canceled",
    );
}

// ── scope: "session" (the folded-in former `which` tool) ─────────────────────

/// Seed one account in the canceled-after-login shape: its stored token still
/// claims `pro` (written once at login, never refreshed) while its cached
/// `/profile` plan has moved to `Free`.
fn seed_canceled_account() {
    let mut profile = Profile::new("kerry".to_string(), None, None);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-kerry".to_string(),
            refresh_token: Some("rt-kerry".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: Some("pro".to_string()),
        }),
    });
    save_profile(&profile).expect("save profile");
    save_app_state(&AppState {
        active_profile: Some("kerry".into()),
        profiles: vec!["kerry".into()],
        ..Default::default()
    })
    .expect("save state");

    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    };
    write_profile_cache(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
        &usage,
    );
}

/// The session row resolves through the same tiers `which` used and reports
/// `source`, on a row rendered by the roster's own `profile_line` — which is
/// what makes the tier read the cached plan, not the login claim
/// (`profile_json::tier_label`, the same helper `clauth which --json` and
/// `status.json` call). A canceled account reports the org's post-cancellation
/// tier, never the one its stored token still claims.
#[test]
fn session_scope_resolves_the_tier_through_the_which_tiers() {
    let home = HomeSandbox::new();
    seed_canceled_account();
    // Resolve by runtime dir rather than by loaded credentials: the `session_dir`
    // tier attributes the session from the path alone, so the fixture does not
    // depend on whatever `~/.claude` holds. The `<pid>-<seq>` shape is load
    // bearing — `is_session_id` rejects anything else and the session would fall
    // through unresolved.
    let runtime = home.home().join(".clauth/profiles/kerry/runtime-4242-1");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let _dir = ConfigDirSandbox::new(&home, &runtime);

    let result = call_profiles(None, Some("session"));
    let text = first_text(&result);
    let row = text.lines().next().expect("the session row");
    assert!(
        row.starts_with("- kerry (global active) [anthropic, Free]"),
        "one row, resolved to the seeded account with the CACHED tier: {row}",
    );
    assert!(
        row.contains("source `session_dir`"),
        "the row names how it resolved: {row}",
    );
    // The live-usage fold names the CONFIGURED active profile. Here that is the
    // same account the row resolved to, so its clause would restate the row's
    // own headroom word for word and is dropped — the row's `(global active)` marker
    // already says the two are one account.
    assert!(
        !text.contains("active profile `kerry`"),
        "one account must not spell its headroom twice on one line: {text}",
    );
    assert!(
        row.contains("(global active)"),
        "and what the dropped clause said is still on the row: {row}",
    );
}

/// A GENERIC api-key endpoint — no typed integration, so `provider` is `None`
/// and `provider_label` renders it `generic` — is still an api-key account:
/// the same scheduler leg caches its usage, it has no Anthropic pool, and it has
/// no Anthropic plan tier. A roster keyed on the display label or on
/// `is_third_party` tells the picker both of those are unknown while holding the
/// figures on disk.
#[test]
fn a_generic_api_key_row_reports_its_own_figures_and_claims_no_anthropic_plan() {
    let _home = HomeSandbox::new();
    save_profile(&Profile::new(
        "litellm".to_string(),
        Some("http://127.0.0.1:4000".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save generic api-key profile");
    save_app_state(&AppState {
        active_profile: Some("litellm".into()),
        profiles: vec!["litellm".into()],
        ..Default::default()
    })
    .expect("save state");
    let cache = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("litellm"),
        THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    std::fs::write(&cache, crate::testutil::THIRD_PARTY_CACHE_BYTES).expect("provider cache");

    let row = lines(&call_profiles(None, None)).remove(0);
    assert_eq!(
        row,
        "- litellm (global active) [generic, 127.0.0.1:4000, local endpoint]: \
         no 5h/7d limits; total: 31.45 CNY",
        "the account's own cached figures, no claim about a plan it cannot have, and the \
         locality marker its loopback base url earns — pinned here on a row the production \
         path built from a saved profile, rather than on a hand-written one",
    );
}

/// A real z.ai quota response, captured 2026-08-25 from the operator's `glm`
/// profile's `third_party_cache.json`. Parsed and re-written through the
/// production paths only — the fixture is real bytes so the reader's
/// assumptions are pinned by the wire shape, never hand-built.
///
/// The bars carry `resets_at` stamps, but they are inert on this path: the
/// third-party rendering chain (`windows_payload` -> `third_party_headline`
/// -> the third-party arm of `windows_prose`) never reads `resets_at`, and
/// the only time-derived input it sees is the precomputed `stale` flag —
/// which merely appends the suffix the contains form already tolerates — so
/// the asserted substrings are a pure function of the stats. The countdown
/// clause lives in `windows_prose`'s OAUTH arm only and is unreachable from
/// a third-party row. The asserts stay contains-based on the plan label and
/// each `label pct%` pair as belt-and-braces against any suffix the row gains
/// later (a freshness clause, a tier), never against a countdown.
const CAPTURED_GLM_CACHE: &str = r#"{"is_available":true,"rows":[{"label":"30d","value":"","kind":"heading"},{"label":"search-prime","value":"1","kind":"body"},{"label":"web-reader","value":"0","kind":"body"},{"label":"zread","value":"0","kind":"body"},{"label":"7d tokens","value":"","kind":"heading"},{"label":"GLM-5.3","value":"291.5M","kind":"body"},{"label":"GLM-5.2","value":"0","kind":"body"},{"label":"GLM-4.7","value":"174.4k","kind":"body"},{"label":"total","value":"291.3M  (2.8k calls)","kind":"faint"}],"bars":[{"label":"5h","pct":0.0},{"label":"7d","pct":97.0,"resets_at":"2026-08-28T19:31:30+00:00"},{"label":"30d","pct":1.0,"resets_at":"2026-09-19T19:31:30+00:00","used":1.0,"total":1000.0}],"plan":"pro","best_effort":false}"#;

/// The bars arm of `windows_payload` on the ROSTER's real cache reader: a z.ai
/// profile whose `third_party_cache.json` carries bars renders the headline
/// those bars build — the shape the owner's example (`pro: 5h …, 7d …, 30d …`)
/// is read in — and the `no 5h/7d limits` denial stays dropped. `windows_prose`
/// is shared, so the unit level and the `monitor` quota path already pin the
/// arm; this pins that the row a model receives goes `save_profile` ->
/// `profile_row` -> the cache reader -> `third_party_headline` with no denial
/// spliced in front of the figure.
#[test]
fn a_bars_carrying_z_ai_row_renders_the_headline_alone() {
    let _home = HomeSandbox::new();
    save_profile(&Profile::new(
        "glm".to_string(),
        Some("https://api.z.ai".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save glm");
    save_app_state(&AppState {
        active_profile: Some("glm".into()),
        profiles: vec!["glm".into()],
        ..Default::default()
    })
    .expect("save state");

    let parsed = serde_json::from_str::<crate::providers::ThirdPartyStats>(CAPTURED_GLM_CACHE)
        .expect("the captured z.ai cache parses");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("glm"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &parsed,
    );

    let row = lines(&call_profiles(None, None)).remove(0);
    assert!(
        row.starts_with("- glm (global active) [Z.ai, api.z.ai]: "),
        "the row is the z.ai roster row: {row}",
    );
    assert!(
        row.contains("pro: 5h 0%, 7d 97%, 30d 1%"),
        "the headline renders the plan label and each bar pair from the real \
         cache reader: {row}",
    );
    assert!(
        !row.contains("no 5h/7d limits"),
        "a bars-publishing provider HAS the limits, so the denial must not \
         ride its figure: {row}",
    );
}

/// The two-wallet ruling (owner 2026-08-28) on the roster's RENDERED figure: a
/// profile whose cache carries the empty USD wallet first must report the
/// funded wallet's figure on the row a model reads — the empty wallet's `0.00
/// USD` is what once read a live account as dead. Driven from the captured
/// cache bytes through the production cache writer and the real `profiles`
/// reply.
#[test]
fn a_two_wallet_profile_renders_its_funded_wallet_figure() {
    let _home = HomeSandbox::new();
    save_profile(&Profile::new(
        "tw".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save tw");
    save_app_state(&AppState {
        active_profile: Some("tw".into()),
        profiles: vec!["tw".into()],
        ..Default::default()
    })
    .expect("save state");
    crate::testutil::write_captured_third_party_cache(
        "tw",
        crate::testutil::CAPTURED_TWO_WALLET_DS_CACHE,
    );

    let row = lines(&call_profiles(None, None)).remove(0);
    assert!(
        row.contains("api balance: 498.18 CNY"),
        "the funded wallet is the rendered figure: {row}",
    );
    assert!(
        !row.contains("0.00 USD"),
        "the empty wallet must not render: {row}",
    );
}

/// One-wallet control for the ruling: a profile whose cache carries a single
/// funded wallet renders exactly as it did before the rule — same figure,
/// same row shape.
#[test]
fn a_single_wallet_profile_renders_unchanged() {
    let _home = HomeSandbox::new();
    save_profile(&Profile::new(
        "one".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save one");
    save_app_state(&AppState {
        active_profile: Some("one".into()),
        profiles: vec!["one".into()],
        ..Default::default()
    })
    .expect("save state");
    crate::testutil::write_captured_third_party_cache(
        "one",
        crate::testutil::CAPTURED_ONE_WALLET_DS_CACHE,
    );

    let row = lines(&call_profiles(None, None)).remove(0);
    assert!(
        row.contains("api balance: 3640.55 CNY"),
        "the single wallet is the rendered figure, as before: {row}",
    );
    assert!(
        row.starts_with("- one (global active) [DeepSeek, api.deepseek.com]: "),
        "the row's identity half is untouched by the ruling: {row}",
    );
}

/// The same refusal on the PRODUCTION path: a `base_url` carrying credentials
/// reaches the roster through `save_profile` -> `profile_row` -> `profile_line`,
/// and the rendered row must name the real host with no userinfo riding on it.
/// The unit table pins `base_url_host` itself; this pins that the row a model
/// actually receives is built from its output.
#[test]
fn a_userinfo_base_url_puts_no_credentials_on_the_profiles_row() {
    let _home = HomeSandbox::new();
    save_profile(&Profile::new(
        "proxy".to_string(),
        Some("http://admin:hunter2@evil.tld/v1".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save userinfo-bearing profile");
    save_app_state(&AppState {
        active_profile: Some("proxy".into()),
        profiles: vec!["proxy".into()],
        ..Default::default()
    })
    .expect("save state");

    let row = lines(&call_profiles(None, None)).remove(0);
    assert!(
        row.contains("[generic, evil.tld]"),
        "the row names the host the request resolves to: {row}",
    );
    assert!(
        !row.contains("hunter2") && !row.contains("admin") && !row.contains('@'),
        "no userinfo on a model-facing row: {row}",
    );
}

/// The roster's `host` reads both endpoint halves. An account whose endpoint
/// exists only as an operator-authored `[env] ANTHROPIC_BASE_URL` has no
/// managed `base_url`, and a row reading the managed field alone renders it
/// as a plain Anthropic account with no host at all, which is the one place
/// the cost model asks whether the host is loopback or LAN.
#[test]
fn an_env_authored_endpoint_renders_its_host_in_the_roster_row() {
    let _home = HomeSandbox::new();
    let mut envhost = Profile::new("envhost".to_string(), None, None);
    envhost.env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "http://localhost:4000/v1".to_string(),
    );
    save_profile(&envhost).expect("save env-endpoint profile");
    save_app_state(&AppState {
        active_profile: Some("envhost".into()),
        profiles: vec!["envhost".into()],
        ..Default::default()
    })
    .expect("save state");

    let rows = lines(&call_profiles(None, None));
    assert_eq!(
        rows,
        vec![
            "- envhost (global active) [anthropic, localhost:4000, local endpoint]: \
             usage unknown; tier unknown"
                .to_string()
        ],
        "the env-authored host renders in the row, locality marker included",
    );
}

/// Finding 11: the retired `which` prose mapped a null tier to `unknown`
/// unconditionally, while `profile_line` guards the same null on the headroom
/// payload's kind (`third_party`) — a third-party account has no plan tier to
/// lose, whatever its provider label says. The session row inherits that guard
/// by being rendered through `profile_line`, and this is what holds the
/// inheritance: a row built any other way says `tier unknown` about an account
/// that structurally has none.
#[test]
fn a_third_party_session_row_claims_no_unknown_it_structurally_has_none_of() {
    let home = HomeSandbox::new();
    save_profile(&Profile::new(
        "vendor".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    ))
    .expect("save third-party profile");
    save_app_state(&AppState {
        active_profile: Some("vendor".into()),
        profiles: vec!["vendor".into()],
        ..Default::default()
    })
    .expect("save state");
    let runtime = home.home().join(".clauth/profiles/vendor/runtime-4242-1");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let _dir = ConfigDirSandbox::new(&home, &runtime);

    let text = first_text(&call_profiles(None, Some("session")));
    let row = text.lines().next().expect("the session row");
    assert!(
        row.starts_with("- vendor (global active) [DeepSeek, api.deepseek.com]"),
        "the session resolves to the third-party account: {row}",
    );
    assert!(
        !row.contains("tier unknown"),
        "a third-party account has no plan tier, so none is missing: {row}",
    );
    assert!(
        row.contains("usage unknown") && !row.contains("no 5h/7d limits"),
        "and nothing cached about its provider's limits, which is an unknown rather \
         than a none: {row}",
    );
}

/// The session-scope reply carries the switch-effect note and, for the one
/// tier that earns it, the runtime-paths note — through the same renderers the
/// init block uses, so a client that drops the block still sees them
/// (placement rule 3: one renderer, two carriers).
#[test]
fn session_scope_reply_carries_the_session_notes() {
    let home = HomeSandbox::new();
    seed_canceled_account();
    let runtime = home.home().join(".clauth/profiles/kerry/runtime-4242-1");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    // The runtime-paths note renders only when the probe finds a shared entry;
    // an empty tree reads `NothingShared` and earns none, so the fixture poses
    // one copy-mode entry.
    std::fs::write(runtime.join("CLAUDE.md"), "").expect("CLAUDE.md copy");
    let _dir = ConfigDirSandbox::new(&home, &runtime);

    let text = first_text(&call_profiles(None, Some("session")));
    assert!(
        text.contains("switch_profile & this session:"),
        "the switch-effect note rides the reply: {text}",
    );
    assert!(
        text.contains("pinned to `kerry`"),
        "the note names this session's own profile: {text}",
    );
    assert!(
        text.contains("runtime paths:"),
        "an isolated-runtime session earns the runtime-paths note: {text}",
    );
}

/// `resolve_active` returning nothing is an unresolved session — a genuine
/// unknown, not "no profiles configured" — and the reply says so instead of
/// rendering an empty roster.
#[test]
fn an_unresolved_session_reads_unknown_not_empty() {
    let home = HomeSandbox::new();
    // A config dir that is nobody's runtime and holds no matching credentials:
    // every tier misses.
    let foreign = home.home().join("foreign-config");
    std::fs::create_dir_all(&foreign).expect("foreign dir");
    let _dir = ConfigDirSandbox::new(&home, &foreign);

    let text = first_text(&call_profiles(None, Some("session")));
    let row = text.lines().next().expect("the session line");
    assert!(
        row.starts_with("session profile unknown, source unknown"),
        "an unresolved session is an unknown, never `no profiles`: {row}",
    );
}

/// `names` filters the roster; the session scope IS already a one-row answer,
/// so the pair is a cross-mode mistake. Refused by name with the fix — the
/// same boundary rule as `monitor`'s job/state seam — instead of silently
/// ignoring a name the all-scope arm would have refused.
#[test]
fn session_scope_refuses_names_by_name() {
    let _home = HomeSandbox::new();
    let result = call_profiles(Some(vec!["ghost"]), Some("session"));
    assert_eq!(result.is_error, Some(true), "the combination is refused");
    assert_eq!(
        first_text(&result),
        "error: `names` cannot combine with `scope: \"session\"`: the session scope answers the \
         one account this session runs on; drop `names`",
    );
    // An empty list stays the established "same as omitted" spelling, not a
    // refusal: that is the convention `names` itself documents.
    let empty = call_profiles(Some(Vec::new()), Some("session"));
    assert_ne!(
        empty.is_error,
        Some(true),
        "an empty `names` list is omitted"
    );
}
