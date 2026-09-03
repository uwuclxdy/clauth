use super::*;
use crate::profile::{ClaudeCredentials, OAuthToken};
use std::fs;

fn creds(access: &str, refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

#[test]
fn diverged_returns_false_when_either_side_missing() {
    let c = creds("a", Some("r"));
    assert!(!credentials_diverged(None, Some(&c)));
    assert!(!credentials_diverged(Some(&c), None));
    assert!(!credentials_diverged(None, None));
}

#[test]
fn diverged_returns_false_when_tokens_match() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-1"));
    assert!(!credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_access_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-2", Some("refresh-1"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_differs() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", Some("refresh-2"));
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_true_when_refresh_token_disappears() {
    let a = creds("access-1", Some("refresh-1"));
    let b = creds("access-1", None);
    assert!(credentials_diverged(Some(&a), Some(&b)));
}

#[test]
fn diverged_returns_false_when_oauth_block_missing_on_one_side() {
    let with = creds("a", Some("r"));
    let without = ClaudeCredentials {
        claude_ai_oauth: None,
    };
    assert!(!credentials_diverged(Some(&with), Some(&without)));
    assert!(!credentials_diverged(Some(&without), Some(&with)));
}

#[test]
fn classify_link_missing_when_path_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Missing,
    );
}

#[test]
fn classify_link_diverged_when_plain_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&link, b"{}").expect("write live");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

/// macOS reality: Claude Code rewrites `~/.claude/.credentials.json` as a plain-file
/// mirror of the Keychain after every run, replacing clauth's symlink. When the live
/// token still matches the active profile's stored token, that is NOT divergence —
/// classify must report LinkedTo so an ordinary switch doesn't falsely prompt to
/// capture credentials that already match. (Regression: the switch prompt fired on
/// every `clauth <name>` because a plain file was unconditionally Diverged.)
#[test]
fn classify_link_linked_to_when_plain_file_token_matches_stored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let same = serde_json::to_vec(&creds("same-access", Some("same-refresh"))).expect("ser");
    fs::write(&link, &same).expect("write live");
    fs::write(&expected, &same).expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
        "a plain file whose token matches the profile is CC's mirror, not divergence",
    );
}

/// A plain file whose access token DIFFERS from the profile's stored token is a
/// genuine CC re-login / rotation — still Diverged so the capture prompt fires.
#[test]
fn classify_link_diverged_when_plain_file_token_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("live-access", Some("r"))).expect("ser"),
    )
    .expect("write live");
    fs::write(
        &expected,
        serde_json::to_vec(&creds("stored-access", Some("r"))).expect("ser"),
    )
    .expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

/// A degenerate empty access token on both sides is a corrupt/partial write, not
/// a completed login — it must NOT read as `LinkedTo` just because two empty
/// strings compare equal. Matches the completed-login intent of `is_first_login`.
#[test]
fn classify_link_diverged_when_plain_file_access_token_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let empty = serde_json::to_vec(&creds("", Some("r"))).expect("ser");
    fs::write(&link, &empty).expect("write live");
    fs::write(&expected, &empty).expect("write stored");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
        "an empty access token is not a completed login, so it is not a mirror",
    );
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_when_pointing_at_expected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

#[cfg(unix)]
#[test]
fn classify_link_diverged_when_symlink_points_elsewhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let other = tmp.path().join("other.json");
    fs::write(&other, b"{}").expect("write other");
    fs::write(&expected, b"{}").expect("write target");
    std::os::unix::fs::symlink(&other, &link).expect("symlink");
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::Diverged,
    );
}

#[test]
fn first_login_true_when_no_stored_creds_and_plain_oauth_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    assert!(is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_stored_creds_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    fs::write(&expected, b"{}").expect("write stored");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_link_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    assert!(!is_first_login_at(&link, &expected));
}

#[test]
fn first_login_false_when_oauth_block_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    // valid JSON but no OAuth block — mid-flight partial write
    fs::write(&link, b"{}").expect("write");
    assert!(!is_first_login_at(&link, &expected));
}

/// A logged-out CC shell keeps `claudeAiOauth` (just with blanked tokens) plus
/// unrelated keys like `mcpOAuth` — it must NOT classify as a first login, or
/// `adopt_first_login` deletes the live file (no install source to relink a
/// blank profile back to) and `mcpOAuth` is lost with it. Regression for the
/// gap PR #46's shell-awareness left in `is_first_login_at` specifically.
#[test]
fn first_login_false_when_live_is_a_logged_out_shell() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": null,
                "expiresAt": 0,
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write shell");
    assert!(!is_first_login_at(&link, &expected));
}

/// Companion to the shell case above, same seam: a completed login (non-blank
/// access token) with the same foreign `mcpOAuth` key still classifies as a
/// first login, so the shell fix can't over-correct and strand a real login.
#[test]
fn first_login_true_when_live_is_a_completed_login_with_foreign_keys() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    fs::write(
        &link,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "real-access",
                "refreshToken": "real-refresh",
                "expiresAt": 1_700_000_000_000_i64,
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write completed login");
    assert!(is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn first_login_false_when_link_is_symlink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    let store = tmp.path().join("store.json");
    fs::write(
        &store,
        serde_json::to_vec(&creds("a", Some("r"))).expect("ser"),
    )
    .expect("write");
    std::os::unix::fs::symlink(&store, &link).expect("symlink");
    assert!(!is_first_login_at(&link, &expected));
}

#[cfg(unix)]
#[test]
fn classify_link_linked_to_even_when_target_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let link = tmp.path().join(".credentials.json");
    let expected = tmp.path().join("profile.json");
    std::os::unix::fs::symlink(&expected, &link).expect("symlink");
    // target absent (e.g. first-ever link, before save_profile writes it)
    assert_eq!(
        classify_link_at(&link, &expected).expect("classify"),
        LinkState::LinkedTo,
    );
}

// ── account-change `[Y/n]` overwrite path ──────────────────────────────────
//
// When Claude Code re-logged into a different account while clauth was closed,
// the live `~/.claude/.credentials.json` is a plain file diverging from the
// active profile's stored chain. clauth shows a `[Y/n]` prompt before the
// stored tokens are overwritten. These tests pin the prompt's GATE (when it
// fires) and both BRANCHES (confirm overwrites/captures, cancel is a no-op) at
// the home-derived seam the prompt actually drives, no TTY needed.

// Not `#[cfg(unix)]`: the ungated session-token tests below use HomeSandbox on
// every platform (it writes only a tempdir + files, no symlinks), so gating the
// import broke the Windows test build.
use crate::testutil::HomeSandbox;

/// Seed an active profile `name` with stored credentials, then simulate CC
/// re-logging into a different account: write a plain (non-symlink) live
/// `~/.claude/.credentials.json` carrying `live`. Returns the assembled config.
// Not `#[cfg(unix)]`: writes only plain files, and the ungated session-token
// tests call it on Windows too.
fn seed_relogin_scenario(
    name: &str,
    stored: ClaudeCredentials,
    live: ClaudeCredentials,
) -> AppConfig {
    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.credentials = Some(stored);
    crate::profile::save_profile(&profile).expect("save profile");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser live")).expect("write live");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some(name.into());
    config.state.profiles = vec![name.into()];
    crate::profile::save_app_state(&config.state).expect("persist state");
    config
}

/// The `[Y/n]` prompt's gate: a re-login is a Diverged plain file that is NOT a
/// first login (the profile already has stored creds), so the prompt fires.
#[cfg(unix)]
#[test]
fn relogin_is_diverged_and_not_first_login() {
    let _home = HomeSandbox::new();
    let _config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    assert_eq!(
        classify_credentials_link(&crate::profile::ProfileName::from("active")).expect("classify"),
        LinkState::Diverged,
        "a CC re-login leaves a plain file diverging from the stored chain",
    );
    assert!(
        !is_first_login(&crate::profile::ProfileName::from("active")).expect("first login"),
        "stored creds exist, so this is a re-login overwrite, not a first login",
    );
}

/// Confirm branch (`y`): capture the live re-login into the active profile, then
/// relink. The stored chain is overwritten with the live one and the live path
/// becomes a symlink back to the profile's now-updated credentials.
#[cfg(unix)]
#[test]
fn overwrite_confirm_captures_relogin_into_profile() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    // `y` answer = force-snapshot the live creds into the active profile, relink.
    force_snapshot_active_credentials(&mut config).expect("snapshot");
    force_link_profile_credentials(&crate::profile::ProfileName::from("active")).expect("relink");

    // The profile's stored chain now holds the re-logged tokens.
    let stored = config
        .find(&crate::profile::ProfileName::from("active"))
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.refresh_token());
    assert_eq!(
        stored,
        Some("relogin-refresh"),
        "confirm must overwrite the stored chain with the live re-login",
    );

    // The live path is reconciled back to a symlink into the profile.
    assert_eq!(
        classify_credentials_link(&crate::profile::ProfileName::from("active")).expect("classify"),
        LinkState::LinkedTo,
        "after capture+relink the live path links to the profile's creds",
    );

    // The on-disk profile credentials file carries the re-logged chain too.
    let on_disk: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("active"))
            .expect("profile dir")
            .join("credentials.json"),
    )
    .expect("read stored creds");
    assert_eq!(
        on_disk.refresh_token(),
        Some("relogin-refresh"),
        "the persisted profile credentials must hold the captured chain",
    );
}

/// Cancel branch (`n`): no capture, no relink. The stored chain keeps its old
/// tokens and the live path is left exactly as CC wrote it (untouched).
#[cfg(unix)]
#[test]
fn overwrite_cancel_leaves_stored_and_live_untouched() {
    let _home = HomeSandbox::new();
    let config = seed_relogin_scenario(
        "active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );

    // `n` answer = abort. We perform no snapshot and no relink; assert the
    // pre-prompt state is preserved.
    let stored = config
        .find(&crate::profile::ProfileName::from("active"))
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.refresh_token());
    assert_eq!(
        stored,
        Some("stored-refresh"),
        "cancel must not overwrite the stored chain",
    );

    // The live file CC wrote is still a plain diverged file with its own chain.
    assert_eq!(
        classify_credentials_link(&crate::profile::ProfileName::from("active")).expect("classify"),
        LinkState::Diverged,
        "cancel leaves the live re-login in place (still diverged)",
    );
    let live = read_claude_credentials()
        .expect("read live")
        .expect("live present");
    assert_eq!(
        live.refresh_token(),
        Some("relogin-refresh"),
        "cancel must leave the live re-login bytes untouched",
    );
}

#[test]
fn build_settings_writes_model_knobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → starts from `{}`
    let mut profile = crate::profile::Profile::new("p".to_string(), None, None);
    profile.models = crate::profile::ModelSettings {
        default: Some("opusplan".to_string()),
        opus: Some("claude-opus-4-8[1m]".to_string()),
        sonnet: None,
        haiku: None,
        fable: Some("claude-fable-5".to_string()),
        subagent: Some("claude-haiku-4-5".to_string()),
    };
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert_eq!(v["model"], "opusplan", "default model → top-level `model`");
    assert_eq!(
        v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
        "claude-opus-4-8[1m]"
    );
    assert_eq!(v["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"], "claude-fable-5");
    assert_eq!(v["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "claude-haiku-4-5");
    assert!(
        v["env"].get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none(),
        "an unset tier override writes no env key",
    );
}

/// `ModelSettings::is_empty` is the gate that decides whether a profile with no
/// endpoint and no env is worth writing settings for at all, so a tier missing
/// from it makes that tier's ONLY-set case a silent no-write.
#[test]
fn a_tier_override_alone_is_enough_to_write_settings() {
    let _home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → nothing to merge onto
    let mut profile = crate::profile::Profile::new("p".to_string(), None, None);
    profile.models.fable = Some("claude-fable-5".to_string());
    assert!(
        !profile.models.is_empty(),
        "a lone tier override is not an empty model block",
    );

    crate::profile::save_profile(&profile).expect("save profile");
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert_eq!(v["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"], "claude-fable-5");
}

// A profile with no model config must strip a previous profile's model knobs
// from the base settings.json, so a switch never inherits stale model routing.
#[test]
fn build_settings_clears_stale_model_knobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(
        &base,
        r#"{"model":"opus","env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"old","ANTHROPIC_DEFAULT_FABLE_MODEL":"old","CLAUDE_CODE_SUBAGENT_MODEL":"old","KEEP":"1"}}"#,
    )
    .expect("seed base settings");
    let profile = crate::profile::Profile::new("p".to_string(), None, None); // empty models
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert!(v.get("model").is_none(), "top-level `model` cleared");
    assert!(v["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none());
    assert!(v["env"].get("ANTHROPIC_DEFAULT_FABLE_MODEL").is_none());
    assert!(v["env"].get("CLAUDE_CODE_SUBAGENT_MODEL").is_none());
    assert_eq!(v["env"]["KEEP"], "1", "unrelated env keys are preserved");
}

// ── apiKeyHelper for api-key profiles ─────────────────────────────────────────
//
// `build_claude_settings_json` swaps `env.ANTHROPIC_AUTH_TOKEN` for CC's
// top-level `apiKeyHelper` when a profile carries an api_key, so the raw key
// leaves the settings.json `env` block and the spawned CC process's env. CC
// runs the helper per request and sends its stdout as both `X-Api-Key` and
// `Authorization: Bearer`.

/// An api-key profile writes `apiKeyHelper` at the top level (NOT under `env`),
/// keeps the raw key out of the rendered JSON, and clears `env.ANTHROPIC_AUTH_TOKEN`.
/// The helper string carries the live exe path, the hidden subcommand, and the
/// profile name — the three tokens CC's shell will re-split.
#[test]
fn build_settings_writes_api_key_helper_not_env_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json"); // absent → starts from `{}`
    let profile = crate::profile::Profile::new(
        "acme".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret-DO-NOT-LEAK".to_string()),
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");

    // Top-level `apiKeyHelper` (not nested under `env`).
    let helper = v
        .get("apiKeyHelper")
        .and_then(|h| h.as_str())
        .expect("apiKeyHelper must be a top-level string");
    assert!(
        v["env"].get("apiKeyHelper").is_none(),
        "apiKeyHelper must NOT live under `env` (CC reads it only at the top level)"
    );

    // The helper command carries the exe path, the hidden subcommand, and the
    // profile name — so CC's shell-invocation of clauth can re-derive the key.
    let exe = std::env::current_exe().expect("test-bin current_exe");
    let exe_str = exe.to_string_lossy();
    // Compared through `shell_quote`: on windows it escapes every `\`, so an
    // absolute exe path never appears literally in the helper.
    assert!(
        helper.contains(&shell_quote(&exe_str)),
        "helper ({helper}) must carry the quoted current exe path ({exe_str})"
    );
    assert!(
        helper.contains("__api-key"),
        "helper ({helper}) must carry the hidden subcommand name"
    );
    assert!(
        helper.contains("acme"),
        "helper ({helper}) must carry the profile name"
    );

    // The raw key MUST NOT appear anywhere in the rendered settings.json:
    // not in env, not at the top level, not inside the helper string.
    assert!(
        !json.contains("sk-secret-DO-NOT-LEAK"),
        "raw api_key must not appear in settings.json; got: {json}"
    );
    assert!(
        v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
        "env.ANTHROPIC_AUTH_TOKEN must be absent (the helper replaces it)"
    );
}

/// A profile with no api_key (OAuth, local endpoint) writes NO `apiKeyHelper`
/// and NO `env.ANTHROPIC_AUTH_TOKEN` — bit-identical to the pre-helper stock
/// behavior. A switch from an api-key profile must clear both.
#[test]
fn build_settings_no_api_key_helper_for_non_api_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    // Seed the base with stale keys the way a prior api-key profile would leave
    // behind — the non-api rebuild must strip both.
    fs::write(
        &base,
        r#"{"apiKeyHelper":"/old/bin/helper","env":{"ANTHROPIC_AUTH_TOKEN":"stale","ANTHROPIC_BASE_URL":"https://api.example.com"}}"#,
    )
    .expect("seed base settings");
    // A non-api-key profile: OAuth/login shape. Carries the seeded base_url so
    // the rebuild preserves it (the assertion below pins that unrelated env
    // keys survive — base_url would otherwise be cleared by `match base_url`).
    let profile = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://api.example.com".to_string()),
        None,
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");

    assert!(
        v.get("apiKeyHelper").is_none(),
        "non-api profile must not write apiKeyHelper; got: {json}"
    );
    assert!(
        v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
        "non-api profile must clear env.ANTHROPIC_AUTH_TOKEN"
    );
    // Unrelated base settings survive.
    assert_eq!(
        v["env"]["ANTHROPIC_BASE_URL"], "https://api.example.com",
        "unrelated env keys are preserved"
    );
}

/// Switching from an api-key profile to a base_url-only profile (no api_key)
/// must drop `apiKeyHelper` and `env.ANTHROPIC_AUTH_TOKEN` together — a stale
/// helper pointing at the old profile would route the new session's requests
/// through the old account.
#[test]
fn build_settings_switch_away_from_api_key_clears_helper() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(
        &base,
        r#"{"apiKeyHelper":"/old/clauth __api-key oldacct","env":{"ANTHROPIC_AUTH_TOKEN":"sk-old","ANTHROPIC_BASE_URL":"https://old.example.com"}}"#,
    )
    .expect("seed api-key base settings");
    let profile = crate::profile::Profile::new(
        "new".to_string(),
        Some("https://new.example.com".to_string()),
        None,
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build settings");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse settings");
    assert!(v.get("apiKeyHelper").is_none());
    assert!(v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://new.example.com");
}

/// The helper command string shell-quotes a spaces-in-path exe so the system
/// shell re-splits it into three tokens. Unix-only because the quoter branches
/// on `cfg(unix)`; Windows quoting is covered structurally (it wraps the same
/// way) but cmd's grammar is too ambiguous to assert byte-exact.
#[test]
fn build_settings_api_key_helper_shell_quotes_exe_path() {
    #[cfg(unix)]
    {
        let quoted = shell_quote("/home/uwu clxdy/bin/clauth");
        // POSIX single-quote, with `'` inside escaped as `'\''`.
        assert_eq!(quoted, "'/home/uwu clxdy/bin/clauth'");

        // A safe-char-only path (the cargo-installed default) is left unquoted.
        let safe = shell_quote("/home/uwuclxdy/.cargo/bin/clauth");
        assert_eq!(safe, "/home/uwuclxdy/.cargo/bin/clauth");

        // An embedded single-quote closes, escapes, and reopens the outer quote.
        let tricky = shell_quote("/path/with/'/clauth");
        assert_eq!(tricky, "'/path/with/'\\''/clauth'");
    }
    #[cfg(not(unix))]
    {
        // Non-Unix quoter is structurally similar but covered only on Windows
        // targets; this test exists for the positive-control assertion on Unix.
    }
}

/// Profile names are validated to a shell-safe charset, so the helper command
/// never needs to quote them. This pins the fast-path: a regression that
/// started escaping profile names would still pass the round-trip but would
/// drift from CC's documented `/bin/<script>` example shape.
#[test]
fn build_settings_api_key_helper_leaves_profile_name_unquoted() {
    let exe = std::path::Path::new("/usr/local/bin/clauth");
    let cmd =
        build_api_key_helper_command(exe, &crate::profile::ProfileName::from("acme_corp-1.0+@"));
    assert_eq!(
        cmd, "/usr/local/bin/clauth __api-key acme_corp-1.0+@",
        "validated profile names must not be over-quoted"
    );
}

/// A long-lived process (daemon/TUI) that rebuilds settings after an in-place
/// self-update reads `env::current_exe()` as `<path> (deleted)` on Linux. The
/// helper strips that marker so CC execs the installed binary at the same path,
/// not a dead one — otherwise every mint 401s until a fresh process rebuilds.
#[test]
fn build_settings_api_key_helper_strips_deleted_exe_marker() {
    let exe = std::path::Path::new("/home/uwuclxdy/.cargo/bin/clauth (deleted)");
    let cmd = build_api_key_helper_command(exe, &crate::profile::ProfileName::from("acme"));
    assert_eq!(cmd, "/home/uwuclxdy/.cargo/bin/clauth __api-key acme");
}

// ── profile_name_from_helper: structural parse of the helper command string ──
//
// `read_claude_endpoint_config` derives the live api_key by parsing the
// `apiKeyHelper` string the runtime settings.json carries. The parser must
// reject anything that isn't exactly `<exe> __api-key <profile>` — a
// hand-edited helper or a different command shape must NOT trigger a profile
// lookup, or `capture_snapshot` could pull the wrong account's key.

#[test]
fn profile_name_from_helper_parses_our_shape() {
    // The shape `build_api_key_helper_command` emits.
    assert_eq!(
        profile_name_from_helper("/usr/local/bin/clauth __api-key acme"),
        Some("acme".to_string()),
    );
    // Exe path with spaces is shell-quoted; split_whitespace still yields
    // three tokens.
    assert_eq!(
        profile_name_from_helper("'/home/uwu clxdy/bin/clauth' __api-key acme"),
        Some("acme".to_string()),
    );
    // Profile name with every validated charset char round-trips.
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key a_b.c@d+e-f"),
        Some("a_b.c@d+e-f".to_string()),
    );
}

#[test]
fn profile_name_from_helper_rejects_wrong_shape() {
    // Not enough tokens.
    assert_eq!(profile_name_from_helper("/x/clauth"), None);
    assert_eq!(profile_name_from_helper("/x/clauth __api-key"), None);
    assert_eq!(profile_name_from_helper(""), None);
    // Too many tokens — a future shape with flags after the name is NOT ours.
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key acme --flag"),
        None,
    );
    // Middle token isn't our subcommand name.
    assert_eq!(
        profile_name_from_helper("/custom/helper acme"),
        None,
        "a foreign helper must not trigger a profile lookup"
    );
    assert_eq!(
        profile_name_from_helper("/x/clauth __other-hidden-cmd acme"),
        None,
    );
    // Profile name fails `validate_profile_name`'s charset.
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key bad/name"),
        None,
        "a path-shaped third token must not parse as a profile name"
    );
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key .hidden"),
        None,
        "a leading-dot profile name is rejected by validate_profile_name"
    );
    assert_eq!(
        profile_name_from_helper("/x/clauth __api-key 'quoted'"),
        None,
        "a quoted profile name means it failed validate_profile_name's charset"
    );
}

/// A whitespace-only api_key is treated as absent at the build layer (matching
/// `api_key_for_profile`'s trim-and-filter at the helper end), so the helper
/// is NOT written for it and `cmd_api_key` will fail closed rather than mint
/// a blank credential.
#[test]
fn build_settings_blank_api_key_writes_no_helper() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("settings.json");
    fs::write(&base, r#"{"apiKeyHelper":"/stale/bin/helper"}"#).expect("seed");
    let profile = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://api.example.com".to_string()),
        Some("   ".to_string()),
    );
    let json = build_claude_settings_json(Some(&base), &profile, &[]).expect("build");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert!(
        v.get("apiKeyHelper").is_none(),
        "a whitespace-only api_key must clear the helper, not write one"
    );
    assert!(
        v["env"].get("ANTHROPIC_AUTH_TOKEN").is_none(),
        "a whitespace-only api_key must not write the env var either"
    );
}

// ── logged-out shell detection ────────────────────────────────────────────────
//
// When Claude Code's own token refresh dies it does not delete the live
// `.credentials.json`: it blanks both tokens and zeroes `expiresAt`, keeping
// unrelated keys like `mcpOAuth` — a logged-out shell. A shell still
// classifies Diverged, so without the exemption every guard built on
// "diverged and unsaved" deferred switches behind a TUI decision about an
// empty file.

/// Truth table for [`live_login_is_empty`]: only a login with NO usable token
/// (both absent or blank, or no OAuth block at all) is empty — one live token
/// on either side keeps the login's protections.
#[test]
fn live_login_is_empty_truth_table() {
    // CC's logged-out shell: both tokens blanked.
    assert!(live_login_is_empty(&creds("", Some(""))));
    // Blank access token and no refresh token at all.
    assert!(live_login_is_empty(&creds("", None)));
    // No OAuth block (a file holding only foreign keys like mcpOAuth).
    assert!(live_login_is_empty(&ClaudeCredentials {
        claude_ai_oauth: None,
    }));
    // A live access token alone is a login.
    assert!(!live_login_is_empty(&creds("at-live", None)));
    assert!(!live_login_is_empty(&creds("at-live", Some(""))));
    // A refresh token alone is a login (the access side merely expired).
    assert!(!live_login_is_empty(&creds("", Some("rt-live"))));
    // A full pair is a login.
    assert!(!live_login_is_empty(&creds("at-live", Some("rt-live"))));
}

/// [`live_credentials_are_shell`] is true only for a PARSED empty login: a
/// missing file is not a shell, and an unreadable/non-JSON file is not a shell
/// either (it may be a CC write in progress — "possibly a login" must keep a
/// real login's protections).
#[test]
fn live_credentials_are_shell_requires_a_parsed_empty_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");

    // Missing file: nothing there to call a shell.
    assert!(!live_credentials_are_shell());

    // CC's logged-out shell, foreign keys and all.
    fs::write(
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
    .expect("write shell");
    assert!(live_credentials_are_shell());

    // No OAuth block at all is the same shell.
    fs::write(&live, r#"{"mcpOAuth":{}}"#).expect("write oauth-less file");
    assert!(live_credentials_are_shell());

    // Torn JSON (a write in progress): NOT a shell — guards stay armed.
    fs::write(&live, br#"{"claudeAiOauth":{"accessToken":""#).expect("write torn file");
    assert!(!live_credentials_are_shell());

    // A real login: not a shell.
    fs::write(
        &live,
        serde_json::to_vec(&creds("at-live", Some("rt-live"))).expect("ser live"),
    )
    .expect("write live");
    assert!(!live_credentials_are_shell());
}

/// `force_snapshot_active_credentials` is the shared sink `reconcile_startup`
/// reaches via `default_divergence: Overwrite` with no sibling owner — a
/// logged-out shell in the live slot must never overwrite the profile's real
/// stored login with blanks (recoverable only by re-login). The second half is
/// a positive control: the guard is narrow to shells only, so a REAL diverged
/// login is still captured by the same sink.
#[test]
fn force_snapshot_skips_shell_but_still_captures_real_divergence() {
    let _home = HomeSandbox::new();

    let mut shell_config = seed_relogin_scenario(
        "shell-active",
        creds("stored-access", Some("stored-refresh")),
        creds("", Some("")),
    );
    force_snapshot_active_credentials(&mut shell_config).expect("force snapshot shell");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("shell-active"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("stored-access"),
        "a logged-out shell must never overwrite the stored access token",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("stored-refresh"),
        "a logged-out shell must never overwrite the stored refresh token",
    );

    let mut real_config = seed_relogin_scenario(
        "real-active",
        creds("stored-access", Some("stored-refresh")),
        creds("relogin-access", Some("relogin-refresh")),
    );
    force_snapshot_active_credentials(&mut real_config).expect("force snapshot real");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("real-active"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("relogin-access"),
        "a real diverged login must still be captured by the guard",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("relogin-refresh"),
        "a real diverged login must still be captured by the guard",
    );
}

/// `reconcile_startup`'s non-diverged sink, `snapshot_active_credentials`,
/// used to route a blank (credential-less) active profile's shell-shaped live
/// file through `is_first_login` -> `adopt_first_login`, which deletes the
/// live file to relink it — but a blank profile has no install source, so
/// nothing gets relinked and the live file (with `mcpOAuth`) is simply gone.
/// The 1Hz poll and the divergence prompt both already guard their own adopt
/// call with `live_credentials_are_shell()`; this pins the startup sink to
/// the same behavior via the shared `is_first_login` classification.
#[test]
fn snapshot_skips_shell_on_blank_profile_and_preserves_live_file() {
    let _home = HomeSandbox::new();

    let profile = crate::profile::Profile::new("blank-active".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("blank-active".into());
    config.state.profiles = vec!["blank-active".into()];
    crate::profile::save_app_state(&config.state).expect("persist state");

    let live = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    let shell_json = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "",
            "refreshToken": null,
            "expiresAt": 0,
        },
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    })
    .to_string();
    fs::write(&live, &shell_json).expect("write shell");

    snapshot_active_credentials(&mut config).expect("snapshot");

    assert!(
        live.exists(),
        "a logged-out shell must not be adopted as a first login, so the live file survives",
    );
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&live).expect("read live")).expect("parse");
    assert_eq!(
        after["mcpOAuth"]["some-server"]["accessToken"], "mcp-tok",
        "mcpOAuth must survive untouched — the sink never adopts, so it never rewrites the slot",
    );
    assert!(
        config
            .find(&crate::profile::ProfileName::from("blank-active"))
            .expect("profile")
            .credentials
            .is_none(),
        "nothing was adopted into the blank profile",
    );
}

/// Sibling hole to the shell case: a TOCTOU delete of the live file inside the
/// confirm window, or a dangling symlink, makes `read_claude_credentials`
/// return `Ok(None)`. That absence is not a login either — the sink must skip
/// the capture instead of wiping the stored login down to `None`.
#[test]
fn force_snapshot_skips_an_absent_live_file() {
    let _home = HomeSandbox::new();

    let mut profile = crate::profile::Profile::new("absent-active".to_string(), None, None);
    profile.credentials = Some(creds("stored-access", Some("stored-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("absent-active".into());
    config.state.profiles = vec!["absent-active".into()];
    crate::profile::save_app_state(&config.state).expect("persist state");

    // No live `.credentials.json` written at all: `claude_credentials_path()`
    // does not exist, matching a TOCTOU delete or a dangling symlink.

    force_snapshot_active_credentials(&mut config).expect("force snapshot absent");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("absent-active"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("stored-access"),
        "an absent live file must never overwrite the stored access token",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("stored-refresh"),
        "an absent live file must never overwrite the stored refresh token",
    );
}

// ── CLA-SPLIT: long-lived session token beside the usage OAuth pair ───────────

/// Write a `session-token.json` (static long-lived login) into `name`'s
/// profile dir, as the split-credential fill does.
fn fill_session_token_by_hand(name: &str, access: &str) {
    let dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("profile dir");
    fs::create_dir_all(&dir).expect("mkdir profile");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds(access, None)).expect("ser session token"),
    )
    .expect("write session token");
}

/// The install source is `credentials.json` until a session token appears,
/// then the session token — and never the OAuth pair while it exists.
#[test]
fn install_source_prefers_session_token() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    assert!(!has_session_token(&crate::profile::ProfileName::from(
        "split"
    )));
    assert!(
        install_source_path(&crate::profile::ProfileName::from("split"))
            .expect("source")
            .ends_with("credentials.json")
    );

    fill_session_token_by_hand("split", "oat-access");
    assert!(has_session_token(&crate::profile::ProfileName::from(
        "split"
    )));
    assert!(
        install_source_path(&crate::profile::ProfileName::from("split"))
            .expect("source")
            .ends_with("session-token.json")
    );
}

/// `installed_session_token` answers with exactly the token a switch installs,
/// which is what `clauth which` attributes the live slot by. It has to track
/// `has_session_token`: a mis-filled sidecar (one carrying a refresh token) is
/// never installed, so attributing a profile by it would name an account no
/// session is running as.
#[test]
fn installed_session_token_tracks_what_a_switch_installs() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    assert_eq!(
        installed_session_token(&crate::profile::ProfileName::from("split")),
        None,
        "no sidecar yet"
    );

    fill_session_token_by_hand("split", "oat-access");
    assert_eq!(
        installed_session_token(&crate::profile::ProfileName::from("split")).as_deref(),
        Some("oat-access")
    );

    // CLA-ROLL: a rolling stamp is refresh-less by construction, exactly like
    // the mint, so it attributes the same way — `clauth which` must keep
    // naming a session when the daemon swaps the mint for a rolling bearer.
    // Re-gating this on a mint-only predicate is what would silently turn
    // every rolling session's statusline to `unknown`.
    stamp_rolling_token(
        &crate::profile::ProfileName::from("split"),
        &OAuthToken {
            access_token: "at-rolling".to_string(),
            refresh_token: Some("rt-chain".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
            scopes: Some(vec!["user:profile".into(), "user:inference".into()]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("stamp rolling");
    assert_eq!(
        installed_session_token(&crate::profile::ProfileName::from("split")).as_deref(),
        Some("at-rolling"),
        "a rolling stamp attributes like the mint"
    );

    // Mis-fill: a rotating pair in the sidecar leaves the split disengaged, so
    // the install source is the OAuth pair and there is nothing to attribute by.
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
        .expect("profile dir");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("oat-access", Some("rt-misfill"))).expect("ser sidecar"),
    )
    .expect("write sidecar");
    assert_eq!(
        session_token_status(&crate::profile::ProfileName::from("split")),
        Some(SessionTokenStatus::NotLongLived)
    );
    assert_eq!(
        installed_session_token(&crate::profile::ProfileName::from("split")),
        None,
        "mis-fill installs nothing"
    );

    // A blank access token is Claude Code's logged-out shell, not a login. It
    // must not become an attribution key, or every profile holding a blanked
    // sidecar would answer to the same empty string.
    fill_session_token_by_hand("split", "");
    assert!(
        has_session_token(&crate::profile::ProfileName::from("split")),
        "a blank mint is still long-lived"
    );
    assert_eq!(
        installed_session_token(&crate::profile::ProfileName::from("split")),
        None,
        "blank is not a token"
    );
}

/// The scheduler's re-login leash watches this fingerprint, and every recovery
/// it prescribes lands as a write to ONE of the three files — so each file
/// must move the fingerprint independently, or the recovery that touches the
/// untracked one waits out the full six-hour leash the watch exists to cut
/// short.
#[test]
fn the_credential_fingerprint_tracks_each_of_the_three_files() {
    let _home = HomeSandbox::new();
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("fp")).expect("dir");
    fs::create_dir_all(&dir).expect("mkdir");
    let f0 = credential_fingerprint(&crate::profile::ProfileName::from("fp"));
    assert_eq!(
        f0,
        [None, None, None],
        "an empty profile has no fingerprint"
    );

    fs::write(dir.join("credentials.json"), b"{\"a\":1}").expect("write");
    let f1 = credential_fingerprint(&crate::profile::ProfileName::from("fp"));
    assert_ne!(
        f1, f0,
        "a re-login (credentials.json) moves the fingerprint"
    );

    fs::write(dir.join("session-token.json"), b"{\"b\":22}").expect("write");
    let f2 = credential_fingerprint(&crate::profile::ProfileName::from("fp"));
    assert_ne!(
        f2, f1,
        "a re-mint (session-token.json) moves the fingerprint"
    );

    fs::write(dir.join("session-token.static.json"), b"{\"c\":333}").expect("write");
    let f3 = credential_fingerprint(&crate::profile::ProfileName::from("fp"));
    assert_ne!(
        f3, f2,
        "a hand-restored backup (session-token.static.json) moves the fingerprint"
    );
}

/// Clearing the sidecar is the only exit from the split. It flips the install
/// source back to the OAuth pair, and it is idempotent: the second call reports
/// "nothing to clear" rather than failing, so a repeated `--clear` is harmless.
#[test]
fn clear_session_token_flips_the_install_source_back() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");
    fill_session_token_by_hand("split", "oat-access");
    assert!(
        install_source_path(&crate::profile::ProfileName::from("split"))
            .expect("source")
            .ends_with("session-token.json")
    );

    assert!(
        clear_session_token(&crate::profile::ProfileName::from("split")).expect("clear"),
        "removed one"
    );
    assert!(!has_session_token(&crate::profile::ProfileName::from(
        "split"
    )));
    assert_eq!(
        session_token_status(&crate::profile::ProfileName::from("split")),
        None
    );
    assert!(
        install_source_path(&crate::profile::ProfileName::from("split"))
            .expect("source")
            .ends_with("credentials.json"),
        "install source flips back to the usage pair"
    );
    // The usage OAuth pair is untouched — clearing drops the sidecar only.
    assert_eq!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("split"))
            .expect("reload")
            .credentials
            .and_then(|c| c.access_token().map(str::to_string))
            .as_deref(),
        Some("usage-access")
    );

    assert!(
        !clear_session_token(&crate::profile::ProfileName::from("split")).expect("second clear"),
        "idempotent: nothing left to remove"
    );
}

/// What a clear falls back TO is not the same question as whether it is allowed.
/// The gate passes on EITHER stored credential, so an api-key profile carrying a
/// sidecar clears onto an ABSENT install source: the forcing relink then removes
/// the live slot and, on macOS, signs the Keychain out. Every surface reporting
/// the clear promised "relinked onto its stored OAuth login" in both states until
/// 2026-08-12, so the predicate the copy branches on is pinned here beside the
/// clear itself.
///
/// Tracks the FILE rather than `Profile::credentials`, because the file is what
/// the relink branches on — asserted by removing the store out from under a
/// profile that still claims one in config.
#[test]
fn has_stored_oauth_login_tracks_the_store_the_relink_would_find() {
    let _home = HomeSandbox::new();

    // An api-key profile with a sidecar: clearable, nothing to relink onto.
    let mut api = crate::profile::Profile::new("apionly".to_string(), None, None);
    api.api_key = Some("sk-ant-api-key".to_string());
    crate::profile::save_profile(&api).expect("save api profile");
    fill_session_token_by_hand("apionly", "oat-access");
    assert!(
        !has_stored_oauth_login(&crate::profile::ProfileName::from("apionly")),
        "an api-key profile has no OAuth login for the clear to fall back to"
    );

    // A profile whose OAuth pair IS what the clear falls back to.
    let mut oauth = crate::profile::Profile::new("split".to_string(), None, None);
    oauth.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&oauth).expect("save oauth profile");
    fill_session_token_by_hand("split", "oat-access");
    assert!(has_stored_oauth_login(&crate::profile::ProfileName::from(
        "split"
    )));
    assert!(
        install_source_path(&crate::profile::ProfileName::from("split"))
            .expect("source")
            .ends_with("session-token.json"),
        "the sidecar still outranks it until the clear runs"
    );

    // The file is the authority: dropping the store flips the answer even though
    // the loaded config still carries the credentials.
    std::fs::remove_file(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("remove the store");
    assert!(
        !has_stored_oauth_login(&crate::profile::ProfileName::from("split")),
        "reads the store on disk, which is what the relink resolves"
    );

    // An unknown profile has no directory at all, so it can fall back to nothing.
    assert!(!has_stored_oauth_login(&crate::profile::ProfileName::from(
        "nosuchprofile"
    )));
}

/// A live slot holding the profile's static session token is the designed
/// steady state: LinkedTo (the divergence machinery stays dormant), and a
/// snapshot leaves the clauth-private usage OAuth pair untouched instead of
/// clobbering it with the token just read.
#[test]
fn session_token_live_is_linked_and_snapshot_keeps_usage_oauth() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "split",
        creds("usage-access", Some("usage-refresh")),
        creds("oat-access", None),
    );
    fill_session_token_by_hand("split", "oat-access");

    assert_eq!(
        classify_credentials_link(&crate::profile::ProfileName::from("split")).expect("classify"),
        LinkState::LinkedTo,
        "live slot holding the session token is the steady state, not divergence",
    );

    snapshot_active_credentials(&mut config).expect("snapshot");
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh"),
        "snapshot must never overwrite the usage OAuth pair with the session token",
    );
}

/// A switch to a session-token profile links the LIVE slot to
/// `session-token.json` — the rotating usage pair is never installed, and it
/// survives the switch on disk byte-for-byte.
#[cfg(unix)]
#[test]
fn switch_installs_session_token_not_usage_oauth() {
    let _home = HomeSandbox::new();

    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("at-a", Some("rt-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("usage-access-b", Some("usage-refresh-b")));
    crate::profile::save_profile(&b).expect("save b");
    fill_session_token_by_hand("b", "oat-b");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![a, b],
    };
    config.state.profiles = vec!["a".into(), "b".into()];
    config.state.active_profile = Some("a".into());
    crate::profile::save_app_state(&config.state).expect("persist state");
    force_link_profile_credentials(&crate::profile::ProfileName::from("a")).expect("link a");

    crate::actions::switch_profile(&mut config, &crate::profile::ProfileName::from("b"))
        .expect("switch to b");

    let live_target =
        std::fs::read_link(claude_credentials_path().expect("path")).expect("live is a symlink");
    assert!(
        live_target.ends_with("session-token.json"),
        "the live slot must point at b's session token, got {live_target:?}",
    );
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("b"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read b store");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh-b"),
        "b's usage OAuth pair must survive the switch untouched",
    );
}

// ── CLA-SPLIT-2: the `--setup-token` capture flow's building blocks ───────────

/// The paste validator refuses everything but a clean single-token mint: a
/// broken sidecar signs every session out on first use, so the failure has to
/// happen at the paste, loudly, and without echoing the value.
#[test]
fn validate_setup_token_accepts_a_mint_and_rejects_bad_pastes() {
    let good = format!("sk-ant-oat01-{}", "x".repeat(48));
    assert_eq!(
        validate_setup_token(&format!("  {good}\n")).expect("valid"),
        good,
        "surrounding whitespace trims away"
    );
    assert!(validate_setup_token("").is_err(), "empty paste");
    assert!(validate_setup_token("   \n").is_err(), "blank paste");
    assert!(
        validate_setup_token("api-key-not-a-mint-0123456789012345678901234567890").is_err(),
        "wrong prefix"
    );
    assert!(
        validate_setup_token(&format!("Setup token: {good}")).is_err(),
        "paste with prompt text has interior whitespace"
    );
    assert!(
        validate_setup_token("sk-ant-short").is_err(),
        "truncated paste"
    );
    assert!(
        validate_setup_token(&format!("sk-ant-api03-{}", "z".repeat(48))).is_err(),
        "an API key must be rejected, not installed as the session bearer",
    );
}

/// The helper emits the api key verbatim to stdout, which CC forwards as an
/// `X-Api-Key`/`Authorization` header. An interior control char would inject or
/// malform that header, so a poisoned key must be refused, not minted.
#[test]
fn validate_api_key_rejects_control_and_whitespace() {
    assert!(
        validate_api_key("sk-ant-api03-abc123").is_ok(),
        "a clean key"
    );
    assert!(
        validate_api_key("sk-ant\r\nX-Evil: 1").is_err(),
        "CRLF injection"
    );
    assert!(validate_api_key("sk-ant\ndaemon").is_err(), "bare newline");
    assert!(validate_api_key("sk ant key").is_err(), "interior space");
    assert!(validate_api_key("sk-ant\tkey").is_err(), "tab");
    assert!(validate_api_key("sk-ant\u{0}key").is_err(), "nul");
}

/// Force-snapshot (the divergence-modal "overwrite" and the CLI reconciled
/// switch both reach it) must never capture the live login into a session-token
/// profile's clauth-private usage OAuth pair. Here the live slot holds a FOREIGN
/// login; the guard at the shared sink leaves the stored usage pair intact.
#[test]
fn force_snapshot_never_clobbers_the_session_token_usage_pair() {
    let _home = HomeSandbox::new();
    let mut config = seed_relogin_scenario(
        "split",
        creds("usage-access", Some("usage-refresh")),
        creds("foreign-access", Some("foreign-refresh")),
    );
    fill_session_token_by_hand("split", "oat-access");

    force_snapshot_active_credentials(&mut config).expect("force snapshot");

    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.refresh_token(),
        Some("usage-refresh"),
        "force-snapshot must leave the clauth-private usage OAuth pair untouched",
    );
}

/// The capture writes a sidecar the whole CLA-SPLIT machinery recognises:
/// `has_session_token` flips, the install source re-points, the stamped
/// one-year horizon reads back through `session_token_expiry`, and the file
/// carries credential permissions.
#[test]
fn write_session_token_produces_a_recognised_sidecar() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::Profile::new("cap".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    assert_eq!(
        session_token_status(&crate::profile::ProfileName::from("cap")),
        None,
        "no sidecar yet"
    );

    let now = 1_700_000_000_000_i64;
    let token = format!("sk-ant-oat01-{}", "y".repeat(48));
    let stamped = write_session_token(&crate::profile::ProfileName::from("cap"), &token, now)
        .expect("write sidecar");
    assert_eq!(stamped, now + SETUP_TOKEN_ASSUMED_LIFETIME_MS);

    assert!(has_session_token(&crate::profile::ProfileName::from("cap")));
    assert!(
        install_source_path(&crate::profile::ProfileName::from("cap"))
            .expect("source")
            .ends_with("session-token.json")
    );
    assert_eq!(
        session_token_status(&crate::profile::ProfileName::from("cap")),
        Some(SessionTokenStatus::LongLived(Some(stamped)))
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(
            crate::profile::profile_dir(&crate::profile::ProfileName::from("cap"))
                .expect("dir")
                .join("session-token.json"),
        )
        .expect("meta")
        .permissions()
        .mode();
        assert_eq!(mode & 0o777, 0o600, "sidecar is a credential file");
    }
}

/// A hand-rolled sidecar without `expiresAt` still reports "present, horizon
/// unknown" — never `None` (which would hide the token row entirely).
#[test]
fn session_token_status_distinguishes_missing_from_unstamped() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::Profile::new("hand".to_string(), None, None);
    crate::profile::save_profile(&profile).expect("save profile");
    fill_session_token_by_hand("hand", "oat-access");
    assert_eq!(
        session_token_status(&crate::profile::ProfileName::from("hand")),
        Some(SessionTokenStatus::LongLived(None))
    );
}

// ── #53 review: the split engages only for a genuinely LONG-LIVED token ──────

/// A sidecar mis-filled with a rotating pair (refresh token present) must NOT
/// engage the split: it reads `NotLongLived`, `has_session_token` stays
/// false, and the install source falls back to `credentials.json` exactly as
/// if the sidecar weren't there — installing a dies-in-hours token with no
/// refresher behind it is the failure this detection exists to prevent.
#[test]
fn a_rotating_pair_in_the_sidecar_never_engages_the_split() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("mis".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("mis"))
        .expect("profile dir");
    fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("rotating-access", Some("rotating-refresh")))
            .expect("ser sidecar"),
    )
    .expect("write sidecar");

    assert_eq!(
        session_token_status(&crate::profile::ProfileName::from("mis")),
        Some(SessionTokenStatus::NotLongLived)
    );
    assert!(
        !has_session_token(&crate::profile::ProfileName::from("mis")),
        "the split stays disengaged"
    );
    assert!(
        install_source_path(&crate::profile::ProfileName::from("mis"))
            .expect("source")
            .ends_with("credentials.json"),
        "switches keep installing the rotating pair from credentials.json"
    );
}

/// The macOS steady state, and the reason the exemption is content-based rather
/// than symlink-identity: after a switch, Claude Code rewrites
/// `~/.claude/.credentials.json` as a REGULAR-FILE mirror of the Keychain,
/// clobbering clauth's symlink with identical content. Capturing a `setup-token`
/// sidecar for the ACTIVE profile then flips the install source to
/// `session-token.json`, so classify reads Diverged over that regular file —
/// yet the live OAuth login is fully saved in the profile's `credentials.json`.
/// `live_login_is_stored` must exempt it by CONTENT (a symlink-identity check
/// reads a regular file as unsaved and defers every switch). Runs on every
/// platform — the content path is what makes the fix portable — so a Linux CI
/// exercises the macOS shape the maintainer can't.
#[test]
fn a_regular_file_mirror_of_a_stored_login_is_not_unsaved() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    // CC's regular-file mirror: same OAuth login as the stored credentials.json,
    // written as a plain file (not our symlink).
    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    fs::write(
        &live,
        serde_json::to_vec(&creds("usage-access", Some("usage-refresh"))).expect("ser"),
    )
    .expect("write regular-file mirror");

    // The sidecar capture flips the install source; classify reads Diverged over
    // the regular file (it no longer matches what a switch installs).
    fill_session_token_by_hand("split", "oat-access");
    assert!(
        matches!(
            classify_credentials_link(&crate::profile::ProfileName::from("split"))
                .expect("classify"),
            LinkState::Diverged
        ),
        "the mirror no longer matches the flipped install source"
    );
    assert!(
        live_login_is_stored(&crate::profile::ProfileName::from("split")),
        "…but the mirror's login is saved in credentials.json — not unsaved \
         (a symlink-identity check would read this regular file as unsaved)"
    );

    // A genuine CC re-login (a DIFFERENT token) is the state the gates exist for —
    // it matches neither store, so it is protected.
    fs::write(
        &live,
        serde_json::to_vec(&creds("cc-relogin", Some("cc-rt"))).expect("ser"),
    )
    .expect("write regular re-login");
    assert!(
        !live_login_is_stored(&crate::profile::ProfileName::from("split")),
        "a re-login whose token matches no store must stay protected"
    );

    // Absent live slot: nothing to match, nothing saved.
    fs::remove_file(&live).expect("drop file");
    assert!(!live_login_is_stored(&crate::profile::ProfileName::from(
        "split"
    )));
}

/// The symlink half of the same exemption, and the original 2026-07-21 repro:
/// capturing a sidecar for the ACTIVE profile flips the install source while the
/// live slot is still clauth's symlink into `credentials.json`. classify reads
/// Diverged (the link no longer points at what a switch installs), but a
/// clauth-owned symlink's target IS a profile store by construction, so nothing
/// is unsaved — `live_login_is_stored` exempts it both structurally (it's a
/// symlink) and by content (reading through it yields the stored login).
#[cfg(unix)]
#[test]
fn a_clauth_symlink_under_a_flipped_install_source_is_not_unsaved() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("split".to_string(), None, None);
    profile.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let live = claude_credentials_path().expect("creds path");
    fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    let store = crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
        .expect("dir")
        .join("credentials.json");
    std::os::unix::fs::symlink(&store, &live).expect("symlink live");
    assert!(
        matches!(
            classify_credentials_link(&crate::profile::ProfileName::from("split"))
                .expect("classify"),
            LinkState::LinkedTo
        ),
        "before the capture the link points at the install source"
    );

    fill_session_token_by_hand("split", "oat-access");
    assert!(
        matches!(
            classify_credentials_link(&crate::profile::ProfileName::from("split"))
                .expect("classify"),
            LinkState::Diverged
        ),
        "the stale link no longer points at what a switch installs"
    );
    assert!(
        live_login_is_stored(&crate::profile::ProfileName::from("split")),
        "…but a clauth-owned symlink holds nothing unsaved"
    );

    // A dangling clauth symlink (its store file removed) still has no login to
    // protect — the structural half keeps exempting it, so a switch is never
    // deferred over an empty slot.
    fs::remove_file(&store).expect("drop store file");
    assert!(
        live_login_is_stored(&crate::profile::ProfileName::from("split")),
        "a dangling clauth symlink is a store slot, not an unsaved login"
    );
}

// ---------------------------------------------------------------------------
// mcpOAuth preservation. `~/.claude/.credentials.json` also holds each MCP
// server's OAuth login (`mcpOAuth`), which is independent of the Claude account;
// an account switch must not drop them. Every token below is synthetic.
// ---------------------------------------------------------------------------

/// A synthetic live-credentials body: a Claude login plus one MCP-server login.
fn live_with_mcp(login: &str, mcp_token: &str) -> serde_json::Value {
    serde_json::json!({
        "claudeAiOauth": { "accessToken": login },
        "mcpOAuth": { "linear": { "accessToken": mcp_token } }
    })
}

#[test]
fn carry_copies_mcp_oauth_and_leaves_the_target_login_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("live.json");
    let target = dir.path().join("credentials.json");
    fs::write(
        &live,
        serde_json::to_vec(&live_with_mcp("live-login", "mock-linear")).unwrap(),
    )
    .expect("write live");
    // Target store is a fresh browser login: a Claude login, no mcpOAuth.
    fs::write(
        &target,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "target-login" } }),
        )
        .unwrap(),
    )
    .expect("write target");

    carry_live_extra_into(
        &live,
        &target,
        &crate::profile::ProfileName::from("carrytest"),
    )
    .expect("carry");

    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&target).expect("read target")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "mcpOAuth carried onto the incoming profile"
    );
    assert_eq!(
        got["claudeAiOauth"]["accessToken"], "target-login",
        "the incoming account's own login is never overwritten by the live one"
    );
}

/// The accepted ceiling, pinned end-to-end rather than at the helper: the carry
/// can add and overwrite, never delete, so a block the live file lacks survives
/// onto the live slot when that store becomes live. Pruning instead would wipe
/// real logins the first time a freshly-logged-in account went live. Anyone who
/// adds pruning fails here and has to go read why it is deliberate.
#[cfg(unix)]
#[test]
fn a_block_the_live_file_lacks_survives_onto_the_live_slot() {
    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("login-b", Some("refresh-b")));
    crate::profile::save_profile(&b).expect("save b");

    // B's store already holds an MCP login from an earlier era; the live file
    // (A, freshly logged in through the browser) carries none.
    let b_store = crate::profile::profile_dir(&crate::profile::ProfileName::from("b"))
        .expect("dir")
        .join("credentials.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&b_store).expect("read b")).expect("parse");
    stored["mcpOAuth"] = serde_json::json!({ "sentry": { "accessToken": "mock-sentry" } });
    fs::write(&b_store, serde_json::to_vec(&stored).unwrap()).expect("seed b");
    force_link_profile_credentials(&crate::profile::ProfileName::from("a")).expect("link a");

    force_link_profile_credentials(&crate::profile::ProfileName::from("b")).expect("link b");

    let live_path = claude_credentials_path().expect("creds path");
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read after")).expect("parse");
    assert_eq!(
        after["mcpOAuth"]["sentry"]["accessToken"], "mock-sentry",
        "a login-only live file must not prune the incoming store's own blocks"
    );
}

/// The static-token sidecar is built by [`write_session_token`] from the mint
/// alone, so anything carried into it is dropped at the next re-mint. Driven
/// through the production writer on purpose: the sidecar DOES carry a
/// `claudeAiOauth` block, so a content-shaped guard reads it as an OAuth store
/// and writes MCP secrets into it.
#[test]
fn carry_skips_the_static_token_sidecar() {
    let _home = HomeSandbox::new();
    let mut split = crate::profile::Profile::new("split".to_string(), None, None);
    split.credentials = Some(creds("usage-access", Some("usage-refresh")));
    crate::profile::save_profile(&split).expect("save split");
    let target = crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
        .expect("dir")
        .join("session-token.json");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("split"),
        &format!("sk-ant-{}", "m".repeat(40)),
        0,
    )
    .expect("mint");
    let before = fs::read(&target).expect("read sidecar");

    let live = crate::profile::profile_dir(&crate::profile::ProfileName::from("split"))
        .expect("dir")
        .join("live.json");
    fs::write(
        &live,
        serde_json::to_vec(&live_with_mcp("live-login", "mock-linear")).unwrap(),
    )
    .expect("write live");

    carry_live_extra_into(
        &live,
        &target,
        &crate::profile::ProfileName::from("carrytest"),
    )
    .expect("carry");

    assert_eq!(
        fs::read(&target).expect("read sidecar"),
        before,
        "the sidecar is rebuilt from the mint on every re-mint, so nothing may be carried into it"
    );
}

/// The carry is an allowlist: only `mcpOAuth` moves. Any other non-login key
/// Claude Code parks in that store stays with the account that minted it.
#[test]
fn carry_moves_only_the_allowlisted_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("live.json");
    let target = dir.path().join("credentials.json");
    fs::write(
        &live,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "live-login" },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } },
            "trustedDeviceToken": "mock-device-token"
        }))
        .unwrap(),
    )
    .expect("write live");
    fs::write(
        &target,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "target-login" } }),
        )
        .unwrap(),
    )
    .expect("write target");

    carry_live_extra_into(
        &live,
        &target,
        &crate::profile::ProfileName::from("carrytest"),
    )
    .expect("carry");

    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&target).expect("read target")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the allowlisted key is carried"
    );
    assert!(
        got.get("trustedDeviceToken").is_none(),
        "an unrecognised key must not cross accounts on a switch"
    );
}

/// The carry's value-level core reports whether it changed anything, and the
/// file path spends a write only when it did. A key the target already holds at
/// the same value is not a change, so an unchanged store is left alone.
#[test]
fn carrying_a_key_the_target_already_holds_reports_no_change() {
    let live = serde_json::json!({ "mcpOAuth": { "linear": { "accessToken": "same" } } });
    let mut target = serde_json::json!({
        "claudeAiOauth": { "accessToken": "target-login" },
        "mcpOAuth": { "linear": { "accessToken": "same" } }
    });

    let changed = carry_live_extra_over(
        target.as_object_mut().expect("target object"),
        live.as_object().expect("live object"),
    );

    assert!(!changed, "an identical block is not a change to write back");
    assert_eq!(
        target["mcpOAuth"]["linear"]["accessToken"], "same",
        "and the block is still there"
    );
}

/// A sign-out drops exactly what belongs to one Claude account and keeps what
/// belongs to none, which is the line Claude Code's own logout draws. Every key
/// is asserted by name: dropping one from
/// the list would otherwise leave the outgoing account's block serving the next.
#[test]
fn a_sign_out_drops_the_account_keys_and_keeps_the_mcp_logins() {
    let mut blob = serde_json::json!({
        "claudeAiOauth": { "accessToken": "outgoing-login" },
        "organizationUuid": "org-1",
        "trustedDeviceToken": "device-1",
        "enterpriseGateway": { "url": "https://gw.example" },
        "designOauth": { "accessToken": "design-1" },
        "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
    });

    assert_eq!(
        strip_account_credentials(&mut blob),
        SignOut::Write,
        "an item still holding MCP logins is written back stripped"
    );

    for key in [
        "claudeAiOauth",
        "organizationUuid",
        "trustedDeviceToken",
        "enterpriseGateway",
        "designOauth",
    ] {
        assert!(
            blob.get(key).is_none(),
            "'{key}' is account-scoped and must not survive a sign-out"
        );
    }
    assert_eq!(
        blob["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "MCP-server logins belong to no account and stay"
    );
}

/// An item holding nothing but the login has nothing left to keep, and the
/// caller deletes it rather than leaving an empty husk where a clean absence was.
#[test]
fn a_sign_out_over_a_login_only_item_leaves_nothing_to_keep() {
    let mut login_only = serde_json::json!({ "claudeAiOauth": { "accessToken": "outgoing" } });
    assert_eq!(strip_account_credentials(&mut login_only), SignOut::Delete);

    let mut not_an_object = serde_json::json!("torn");
    assert_eq!(
        strip_account_credentials(&mut not_an_object),
        SignOut::Delete,
        "an item that is not an object carries nothing worth preserving"
    );
}

/// A store already carrying no account-scoped key is already signed out, and
/// says so instead of asking for a rewrite of identical bytes. The daemon and
/// the TUI relink the active profile on a tick, so on macOS the difference is a
/// `security` subprocess per tick against none.
#[test]
fn a_store_with_no_account_keys_is_already_signed_out() {
    let mut mcp_only =
        serde_json::json!({ "mcpOAuth": { "linear": { "accessToken": "mock-linear" } } });

    assert_eq!(strip_account_credentials(&mut mcp_only), SignOut::Nothing);
    assert_eq!(
        mcp_only["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "and it is left exactly as it was"
    );
}

/// The carry is a new writer of a file under `~/.clauth`, so it owes the tree's
/// 0600 invariant like every other one.
#[cfg(unix)]
#[test]
fn carry_keeps_the_store_at_0600() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let target = crate::profile::profile_dir(&crate::profile::ProfileName::from("a"))
        .expect("dir")
        .join("credentials.json");

    let live = crate::profile::profile_dir(&crate::profile::ProfileName::from("a"))
        .expect("dir")
        .join("live.json");
    fs::write(
        &live,
        serde_json::to_vec(&live_with_mcp("live-login", "mock-linear")).unwrap(),
    )
    .expect("write live");

    carry_live_extra_into(
        &live,
        &target,
        &crate::profile::ProfileName::from("carrytest"),
    )
    .expect("carry");

    // Assert the write HAPPENED before asserting its mode: a carry that never
    // runs leaves the mode `save_profile` set, so a mode check alone passes
    // against a no-op and the posture it claims to pin goes uncovered.
    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&target).expect("read target")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the carry must have rewritten the store for its mode to mean anything"
    );
    let mode = fs::metadata(&target).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the carry must not widen the store's mode");
}

/// Seed `name` with a store carrying a login AND an `mcpOAuth` block, the shape
/// a profile holds once it has been live with an authenticated MCP server.
/// Returns the store path. Built through `save_profile` rather than hand-written
/// so the file this parks out of is the one production writes.
fn seed_store_with_mcp_logins(name: &str) -> std::path::PathBuf {
    // The park the removal below writes is a cache write, gated on the on-disk
    // record — which `save_profile` does not touch.
    crate::testutil::register_names(&[name]);
    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.credentials = Some(creds("stored-access", Some("stored-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");
    let store = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
        .expect("profile dir")
        .join("credentials.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("read store")).expect("parse store");
    value["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    fs::write(&store, serde_json::to_vec(&value).expect("ser")).expect("seed mcpOAuth");
    store
}

/// A profile that stops storing a login has `credentials.json` removed under it,
/// and where the live slot is clauth's symlink that file IS what the slot
/// resolves to. So its MCP-server logins are unreachable at the removal, before
/// any relink runs, and the switch-time carry never sees them. They belong to no
/// Claude account, so dropping them signs the operator out of every MCP server
/// with nothing on the box naming the cause.
#[test]
fn a_store_removal_parks_the_mcp_logins_it_was_holding() {
    let _home = HomeSandbox::new();
    let store = seed_store_with_mcp_logins("crossed");
    let mut profile = crate::profile::Profile::new("crossed".to_string(), None, None);

    profile.credentials = None;
    crate::profile::save_profile(&profile).expect("save without a login");

    assert!(!store.exists(), "the store is still removed");
    let parked: serde_json::Value = crate::profile_cache::load_profile_cache(
        &crate::profile::ProfileName::from("crossed"),
        crate::profile_cache::MCP_LOGINS_FILE,
    )
    .expect("the MCP logins must be parked, not dropped with the store");
    assert_eq!(parked["mcpOAuth"]["linear"]["accessToken"], "mock-linear");
    assert!(
        parked.get("claudeAiOauth").is_none(),
        "the account's own login is not parked with them"
    );
}

/// The other half: a later `clauth login <name>` writes a store again, and the
/// parked block goes back into it, because that store is where every reader of
/// `mcpOAuth` looks. The parked copy is dropped once the merged write lands, so
/// nothing keeps re-attaching a block the carry now maintains normally.
#[test]
fn a_regained_store_takes_its_parked_mcp_logins_back() {
    let _home = HomeSandbox::new();
    let store = seed_store_with_mcp_logins("crossed");
    let mut profile = crate::profile::Profile::new("crossed".to_string(), None, None);
    profile.credentials = None;
    crate::profile::save_profile(&profile).expect("save without a login");

    profile.credentials = Some(creds("fresh-access", Some("fresh-refresh")));
    crate::profile::save_profile(&profile).expect("save the recapture");

    let got: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("read store")).expect("parse");
    assert_eq!(
        got["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the parked MCP logins belong in the store the profile just regained"
    );
    assert_eq!(
        got["claudeAiOauth"]["accessToken"], "fresh-access",
        "and the restore must not overwrite the login the capture just wrote"
    );
    assert!(
        crate::profile_cache::load_profile_cache::<serde_json::Value>(
            &crate::profile::ProfileName::from("crossed"),
            crate::profile_cache::MCP_LOGINS_FILE
        )
        .is_none(),
        "the parked copy goes once the store holds them again"
    );
}

/// The second way the block goes unreachable, and the reason the park is not
/// scoped to the removal alone: switching onto a profile that stores no login at
/// all (an api-key or third-party account) leaves the carry no store to land in,
/// and the relink then drops the live slot.
#[test]
fn a_carry_with_no_store_to_land_in_parks_instead() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let live = dir.path().join("live.json");
    fs::write(
        &live,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "live-login" },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } },
            "trustedDeviceToken": "mock-device-token"
        }))
        .expect("ser"),
    )
    .expect("write live");
    let absent = dir.path().join("credentials.json");

    // The park the carry falls back to is a cache write, gated on the record.
    crate::testutil::register_names(&["apikey"]);
    carry_live_extra_into(&live, &absent, &crate::profile::ProfileName::from("apikey"))
        .expect("carry");

    let parked: serde_json::Value = crate::profile_cache::load_profile_cache(
        &crate::profile::ProfileName::from("apikey"),
        crate::profile_cache::MCP_LOGINS_FILE,
    )
    .expect("a carry with nowhere to land must park rather than drop");
    assert_eq!(parked["mcpOAuth"]["linear"]["accessToken"], "mock-linear");
    assert!(
        parked.get("trustedDeviceToken").is_none(),
        "the allowlist bounds what is parked exactly as it bounds what is carried"
    );
}

/// An absent parked file and an empty one must not both mean "parked, and there
/// was nothing": the restore re-attaches whatever the parked file holds, so an
/// empty one written on every login-less save would be a standing no-op file
/// under every api-key profile.
#[test]
fn a_store_with_no_mcp_logins_parks_nothing() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("plain".to_string(), None, None);
    profile.credentials = Some(creds("stored-access", Some("stored-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    profile.credentials = None;
    crate::profile::save_profile(&profile).expect("save without a login");

    assert!(
        crate::profile_cache::profile_cache_path(
            &crate::profile::ProfileName::from("plain"),
            crate::profile_cache::MCP_LOGINS_FILE
        )
        .is_some_and(|p| !p.exists()),
        "a store carrying no MCP logins parks no file at all"
    );
}

/// `~/.clauth` is 0600 files / 0700 dirs whole-tree, and a new writer that
/// reverts to the umask is the way that has been broken before. Asserts the park
/// HAPPENED first: a park that never runs leaves no file, and a mode check over
/// an absent file cannot fail for the reason this test is named for.
#[cfg(unix)]
#[test]
fn the_parked_mcp_logins_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    seed_store_with_mcp_logins("crossed");
    let mut profile = crate::profile::Profile::new("crossed".to_string(), None, None);
    profile.credentials = None;
    crate::profile::save_profile(&profile).expect("save without a login");

    let parked = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("crossed"),
        crate::profile_cache::MCP_LOGINS_FILE,
    )
    .expect("parked path");
    assert!(
        parked.exists(),
        "the park must have written for its mode to mean anything"
    );
    let mode = fs::metadata(&parked).expect("stat").permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "a parked file is owner-only like every ~/.clauth file"
    );
}

#[cfg(unix)]
#[test]
fn switching_accounts_preserves_mcp_oauth_end_to_end() {
    let _home = HomeSandbox::new();

    // Two OAuth profiles, each login-only in its store (as a browser login lands).
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("login-b", Some("refresh-b")));
    crate::profile::save_profile(&b).expect("save b");

    // Make A live, then simulate Claude Code authenticating an MCP server: it
    // writes an mcpOAuth block through clauth's symlink into A's store.
    force_link_profile_credentials(&crate::profile::ProfileName::from("a")).expect("link a");
    let live_path = claude_credentials_path().expect("creds path");
    let mut live: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read live")).expect("parse live");
    live["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    fs::write(&live_path, serde_json::to_vec(&live).unwrap()).expect("write live mcp");

    // Switch to B.
    force_link_profile_credentials(&crate::profile::ProfileName::from("b")).expect("link b");

    // The live credential now resolves to B's login AND still carries mcpOAuth.
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read after")).expect("parse after");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"], "login-b",
        "the switch installed account B's login"
    );
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "the MCP-server login survived the account switch"
    );
}

#[test]
fn link_adopts_a_matching_login_and_preserves_mcp_oauth() {
    let _home = HomeSandbox::new();
    // Profile "main" holds a login-only store — exactly how a snapshot of the live
    // account records it, since the typed model drops mcpOAuth.
    let mut main = crate::profile::Profile::new("main".to_string(), None, None);
    main.credentials = Some(creds("acct-login", Some("acct-refresh")));
    crate::profile::save_profile(&main).expect("save main");

    // The live file is the SAME account (an untracked regular file) carrying an
    // mcpOAuth block — the state that made the byte-compare guard falsely refuse.
    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "acct-login" },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
        }))
        .unwrap(),
    )
    .expect("write live");

    // Must NOT refuse (same login), and must carry mcpOAuth onto the store.
    link_profile_credentials(&crate::profile::ProfileName::from("main"))
        .expect("link adopts a matching login");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read after")).expect("parse");
    assert_eq!(after["claudeAiOauth"]["accessToken"], "acct-login");
    assert_eq!(
        after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
        "mcpOAuth survived the adoption link"
    );
}

#[test]
fn link_still_refuses_a_different_live_login() {
    let _home = HomeSandbox::new();
    let mut other = crate::profile::Profile::new("other".to_string(), None, None);
    other.credentials = Some(creds("other-login", Some("other-refresh")));
    crate::profile::save_profile(&other).expect("save other");

    // Live is an unresolved DIFFERENT account — a CC re-login the user hasn't saved.
    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "unsaved-live-login" } }),
        )
        .unwrap(),
    )
    .expect("write live");

    let err = link_profile_credentials(&crate::profile::ProfileName::from("other"))
        .expect_err("must refuse a different login");
    assert!(
        err.to_string().contains("refusing to replace"),
        "the guard still protects an unresolved different login: {err}"
    );
}

/// The refuse-guard compares logins, so it must not read "neither side names a
/// login" as "the logins match". A live file too torn to parse yields no login,
/// and so does an install source that does not exist — which is every profile
/// storing no `credentials.json`. Left to the login test alone the two compare
/// equal, the guard clears, and the live file is removed with nothing to relink.
/// The byte-compare fallback is what keeps refusing here.
#[test]
fn link_refuses_a_torn_live_file_over_a_profile_storing_no_login() {
    let _home = HomeSandbox::new();
    // An api-key profile: saved, and storing no credentials.json at all.
    let mut endpoint = crate::profile::Profile::new(
        "endpoint".to_string(),
        Some("https://api.example.invalid".to_string()),
        Some("mock-key".to_string()),
    );
    endpoint.credentials = None;
    crate::profile::save_profile(&endpoint).expect("save endpoint");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    // Caught mid-write by CC: valid prefix, no closing brace.
    std::fs::write(&live_path, br#"{"claudeAiOauth":{"accessToken":"live"#).expect("write torn");

    let err = link_profile_credentials(&crate::profile::ProfileName::from("endpoint"))
        .expect_err("must refuse a torn live file");
    assert!(
        err.to_string().contains("refusing to replace"),
        "a file too torn to parse is a possible mid-write login, not a match: {err}"
    );
    assert!(
        live_path.exists(),
        "the torn live file must survive the refusal, not be deleted with nothing to relink"
    );
}

/// Same hole, reached by the other route: a live file carrying MCP-server logins
/// and no Claude login block parses fine and still names no login.
#[test]
fn link_refuses_a_login_less_live_file_over_a_profile_storing_no_login() {
    let _home = HomeSandbox::new();
    let mut endpoint = crate::profile::Profile::new(
        "endpoint".to_string(),
        Some("https://api.example.invalid".to_string()),
        Some("mock-key".to_string()),
    );
    endpoint.credentials = None;
    crate::profile::save_profile(&endpoint).expect("save endpoint");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&serde_json::json!({
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
        }))
        .unwrap(),
    )
    .expect("write login-less live");

    let err = link_profile_credentials(&crate::profile::ProfileName::from("endpoint"))
        .expect_err("must refuse a login-less live file");
    assert!(
        err.to_string().contains("refusing to replace"),
        "no login on either side is not a matching login: {err}"
    );
    assert!(
        live_path.exists(),
        "the MCP blocks must survive the refusal — deleting them is the loss this feature exists to stop"
    );
}

/// Two logged-out shells are two blank tokens, and blank equals blank. The login
/// test carries `classify_link_at`'s non-empty clause so it never clears on them;
/// differing shells then fall to the byte compare and refuse.
#[test]
fn link_refuses_two_differing_logged_out_shells() {
    let _home = HomeSandbox::new();
    let mut acct = crate::profile::Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(creds("", Some("")));
    crate::profile::save_profile(&acct).expect("save acct");

    let live_path = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live_path,
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": { "accessToken": "", "refreshToken": "", "expiresAt": 0 },
            "mcpOAuth": { "linear": { "accessToken": "mock-linear" } }
        }))
        .unwrap(),
    )
    .expect("write shell");

    let err = link_profile_credentials(&crate::profile::ProfileName::from("acct"))
        .expect_err("must refuse two blank logins");
    assert!(
        err.to_string().contains("refusing to replace"),
        "a blank token must never match another blank token: {err}"
    );
}

/// A carry failure must not strand the operator on the outgoing account.
/// Preserving MCP logins is a convenience; completing the switch is not, so an
/// unwritable profile directory reports and continues.
#[cfg(unix)]
#[test]
fn a_failed_carry_still_completes_the_switch() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let mut a = crate::profile::Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("login-a", Some("refresh-a")));
    crate::profile::save_profile(&a).expect("save a");
    let mut b = crate::profile::Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("login-b", Some("refresh-b")));
    crate::profile::save_profile(&b).expect("save b");

    force_link_profile_credentials(&crate::profile::ProfileName::from("a")).expect("link a");
    let live_path = claude_credentials_path().expect("creds path");
    let mut live: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read live")).expect("parse live");
    live["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
    fs::write(&live_path, serde_json::to_vec(&live).unwrap()).expect("write live mcp");

    // Lock B's directory so the carry's atomic write cannot land its temp file.
    let b_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("b")).expect("dir");
    fs::set_permissions(&b_dir, fs::Permissions::from_mode(0o500)).expect("lock b");
    if fs::write(b_dir.join(".probe"), b"x").is_ok() {
        // Running as root: mode bits do not deny, so there is no failure to drive.
        fs::set_permissions(&b_dir, fs::Permissions::from_mode(0o700)).expect("unlock b");
        return;
    }

    let result = force_link_profile_credentials(&crate::profile::ProfileName::from("b"));
    fs::set_permissions(&b_dir, fs::Permissions::from_mode(0o700)).expect("unlock b");
    result.expect("an unwritable store must not fail the switch");

    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&live_path).expect("read after")).expect("parse after");
    assert_eq!(
        after["claudeAiOauth"]["accessToken"], "login-b",
        "the switch installed account B even though its MCP carry could not land"
    );
}

/// The fed sidecar carries the chain's access token, real expiry, scopes, and
/// subscriptionType — and NEVER a refresh token, so the classifier stays
/// LongLived and every split guard keeps working unmodified.
#[test]
fn stamp_rolling_token_writes_a_refreshless_long_lived_shape() {
    let _home = HomeSandbox::new();
    let name = "feed-shape";
    std::fs::create_dir_all(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir"),
    )
    .expect("mkdir");
    let exp = crate::usage::now_ms() as i64 + 8 * 3_600_000;
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-chain".to_string(),
            refresh_token: Some("rt-chain".to_string()),
            expires_at: Some(exp),
            scopes: Some(vec!["user:profile".into(), "user:inference".into()]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let status = session_token_status(&crate::profile::ProfileName::from(name)).expect("sidecar");
    assert_eq!(status, SessionTokenStatus::LongLived(Some(exp)));
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let creds: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
            .expect("parse");
    let oauth = creds.claude_ai_oauth.expect("oauth");
    assert_eq!(oauth.access_token, "at-chain");
    assert!(
        oauth.refresh_token.is_none(),
        "the pair never reaches the sidecar"
    );
    assert_eq!(oauth.subscription_type.as_deref(), Some("max"));
    assert_eq!(
        oauth.scopes.as_deref(),
        Some(&["user:profile".to_string(), "user:inference".to_string()][..])
    );
}

/// First feed preserves a genuine mint exactly once; later feeds leave the
/// backup alone, and a fed (hours-horizon) sidecar is never mistaken for one.
#[test]
fn first_stamp_preserves_the_mint_once_and_only_the_mint() {
    let _home = HomeSandbox::new();
    let name = "feed-preserve";
    std::fs::create_dir_all(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir"),
    )
    .expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-genuine-mint-value-1234567890",
        now,
    )
    .expect("mint");
    let fed = |token: &str| OAuthToken {
        access_token: token.to_string(),
        refresh_token: None,
        expires_at: Some(now + 8 * 3_600_000),
        scopes: None,
        subscription_type: Some("max".into()),
    };
    stamp_rolling_token(&crate::profile::ProfileName::from(name), &fed("at-1")).expect("feed 1");
    stamp_rolling_token(&crate::profile::ProfileName::from(name), &fed("at-2")).expect("feed 2");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(dir.join("session-token.static.json")).expect("backup exists"),
    )
    .expect("parse");
    assert_eq!(
        backup.access_token(),
        Some("sk-ant-oat01-genuine-mint-value-1234567890"),
        "the backup is the ORIGINAL mint, not a fed value"
    );

    // A fed sidecar with the backup consumed is never re-preserved as a mint
    // (subscriptionType + hours horizon both disqualify it).
    std::fs::remove_file(dir.join("session-token.static.json")).expect("consume");
    stamp_rolling_token(&crate::profile::ProfileName::from(name), &fed("at-3")).expect("feed 3");
    assert!(
        !dir.join("session-token.static.json").exists(),
        "a fed value must never become the degrade fallback"
    );
}

/// Restore round-trip: the mint comes back byte-identical, the backup is
/// consumed, and a second restore is a no-op `false`.
#[test]
fn restore_static_mint_round_trip() {
    let _home = HomeSandbox::new();
    let name = "feed-restore";
    std::fs::create_dir_all(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir"),
    )
    .expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-genuine-mint-value-1234567890",
        now,
    )
    .expect("mint");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let mint_bytes = std::fs::read(dir.join("session-token.json")).expect("mint bytes");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(now + 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    assert!(restore_static_mint(&crate::profile::ProfileName::from(name)).expect("restore"));
    assert_eq!(
        std::fs::read(dir.join("session-token.json")).expect("sidecar"),
        mint_bytes,
        "mint restored byte-identical"
    );
    assert!(
        !restore_static_mint(&crate::profile::ProfileName::from(name)).expect("second restore"),
        "backup consumed"
    );
}

/// `write_session_token_with_backup` stamps the fresh mint into the sidecar
/// AND the degrade backup from the same bytes (one flock section — the
/// re-mint-on-a-feed-profile path, immune to a concurrent rotation feed
/// landing between a write and a read-back).
#[test]
fn write_session_token_with_backup_stamps_both_from_the_same_mint() {
    let _home = HomeSandbox::new();
    let name = "feed-remint";
    std::fs::create_dir_all(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir"),
    )
    .expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-genuine-mint-value-1234567890",
        now,
    )
    .expect("mint 1");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(now + 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed preserves mint 1");
    write_session_token_with_backup(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-fresher-mint-value-0987654321",
        now,
    )
    .expect("re-mint");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let sidecar = std::fs::read(dir.join("session-token.json")).expect("sidecar");
    let backup = std::fs::read(dir.join("session-token.static.json")).expect("backup");
    assert_eq!(sidecar, backup, "one mint, two byte-identical copies");
    let parsed: ClaudeCredentials = serde_json::from_slice(&backup).expect("parse");
    assert_eq!(
        parsed.access_token(),
        Some("sk-ant-oat01-fresher-mint-value-0987654321"),
        "the FRESH mint is the degrade fallback now"
    );
}

/// A feed profile's mis-filled sidecar heals when a backup exists: evidence
/// lands in quarantine, the mint comes back, the backup is consumed. Without
/// a backup nothing is touched (`NoLiveBackup` — the disengaged posture).
#[test]
fn heal_misfilled_sidecar_quarantines_and_restores_the_mint() {
    let _home = HomeSandbox::new();
    let name = "feed-heal";
    std::fs::create_dir_all(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir"),
    )
    .expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-genuine-mint-value-1234567890",
        now,
    )
    .expect("mint");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let mint_bytes = std::fs::read(dir.join("session-token.json")).expect("mint bytes");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(now + 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed preserves mint");
    // Something scribbles a rotating pair into the sidecar (the mis-fill).
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("at-misfill", Some("rt-misfill"))).expect("ser"),
    )
    .expect("misfill");
    assert_eq!(
        heal_misfilled_sidecar(&crate::profile::ProfileName::from(name)).expect("heal"),
        HealOutcome::Healed
    );
    assert_eq!(
        std::fs::read(dir.join("session-token.json")).expect("sidecar"),
        mint_bytes,
        "mint restored"
    );
    assert!(
        !dir.join("session-token.static.json").exists(),
        "backup consumed"
    );
    // Under the PROFILE, so `clauth delete` sweeps the rotating pair it holds
    // along with everything else that account owns.
    let quarantine = dir.join("quarantine");
    let quarantined = std::fs::read_dir(&quarantine)
        .expect("quarantine dir")
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with(".session-token.json")
        });
    assert!(quarantined, "mis-fill evidence preserved in quarantine");

    // Second mis-fill, no backup left: nothing healed, nothing touched.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("at-misfill-2", Some("rt-misfill-2"))).expect("ser"),
    )
    .expect("misfill 2");
    assert_eq!(
        heal_misfilled_sidecar(&crate::profile::ProfileName::from(name)).expect("no-backup heal"),
        HealOutcome::NoLiveBackup
    );
    assert!(
        matches!(
            session_token_status(&crate::profile::ProfileName::from(name)),
            Some(SessionTokenStatus::NotLongLived)
        ),
        "mis-fill left in place without a backup"
    );
    // And a sidecar that is not mis-filled at all reports itself as exactly
    // that — the state the install gate lets fall through to the normal
    // rolling table, never the vanilla path.
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-genuine-mint-value-1234567890",
        now,
    )
    .expect("re-mint");
    assert_eq!(
        heal_misfilled_sidecar(&crate::profile::ProfileName::from(name)).expect("healthy heal"),
        HealOutcome::NotMisfilled
    );
}

// ── the mint-vs-rolling discriminator ────────────────────────────────────────
//
// The two are told apart by the SCOPE SET, the one signal every sidecar ever
// written carries: a mint records `SETUP_TOKEN_SCOPES`, a rolling stamp clones
// the chain's grant, and a chain fit to be a usage chain can never carry the
// mint's set (`/api/oauth/usage` 403s exactly those scopes — the #52 root
// cause). The old 30-day-horizon inference was wrong on both sides of itself;
// these pin both failures as fixed, and pin that expiry no longer decides
// anything.

/// A refresh-less token builder for classification fixtures.
fn refreshless(
    scopes: Option<Vec<&str>>,
    plan: Option<&str>,
    expires_at: Option<i64>,
) -> OAuthToken {
    OAuthToken {
        access_token: "at".to_string(),
        refresh_token: None,
        expires_at,
        scopes: scopes.map(|s| s.into_iter().map(String::from).collect()),
        subscription_type: plan.map(String::from),
    }
}

#[test]
fn classification_is_the_scope_set_never_the_clock() {
    let now = crate::usage::now_ms() as i64;
    let mint_scopes = || Some(vec!["user:inference", "user:sessions:claude_code"]);
    let chain_scopes = || Some(vec!["user:inference", "user:profile"]);

    // A mint is a mint at any remaining life — ten days out included, which
    // the old horizon read as rolling and destroyed with no backup.
    for exp in [
        Some(now + 300 * 86_400_000),
        Some(now + 10 * 86_400_000),
        None,
    ] {
        assert_eq!(
            sidecar_kind_of(&refreshless(mint_scopes(), None, exp)),
            SidecarKind::Mint,
            "a mint's remaining life must not reclassify it (exp={exp:?})"
        );
    }
    // A chain grant is rolling at any stamped expiry — a year-scale stamp
    // included, which the old horizon read as a mint. Expiry is hand-editable
    // (`SETUP_TOKEN_ASSUMED_LIFETIME_MS`'s doc invites it); scopes are not.
    for exp in [Some(now + 3_600_000), Some(now + 300 * 86_400_000), None] {
        assert_eq!(
            sidecar_kind_of(&refreshless(chain_scopes(), None, exp)),
            SidecarKind::Rolling,
            "a scope beyond the setup pair proves the chain wrote it (exp={exp:?})"
        );
    }
    // The plan stamp is the belt: no mint write ever sets it.
    assert_eq!(
        sidecar_kind_of(&refreshless(None, Some("max"), Some(now + 3_600_000))),
        SidecarKind::Rolling
    );
    // Absent or subset scopes read Mint — the failure direction that KEEPS a
    // copy. `stamp_rolling_token` refuses to write such a bearer, so only a
    // hand-edit can reach these shapes.
    assert_eq!(
        sidecar_kind_of(&refreshless(None, None, Some(now + 3_600_000))),
        SidecarKind::Mint
    );
    assert_eq!(
        sidecar_kind_of(&refreshless(Some(vec!["user:inference"]), None, None)),
        SidecarKind::Mint
    );
    assert_eq!(
        sidecar_kind_of(&refreshless(Some(vec![]), None, None)),
        SidecarKind::Mint
    );
}

/// The write-site guard that makes the classifier total: the only writer of
/// rolling content refuses a chain grant it could not later tell from a mint,
/// and leaves the sidecar untouched when it does.
#[test]
fn stamp_rolling_token_refuses_a_mint_shaped_chain_grant() {
    let _home = HomeSandbox::new();
    let name = "stamp-refuse";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let err = stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &refreshless(
            Some(vec!["user:inference", "user:sessions:claude_code"]),
            None,
            Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
        ),
    );
    assert!(
        err.is_err(),
        "a mint-classifying grant must never be stamped as rolling"
    );
    assert!(
        !dir.join("session-token.json").exists(),
        "refusal writes nothing"
    );
}

/// Round 1's blocker 3, closed for EVERY sidecar in the field: a real mint in
/// its final month was read as a rolling value and destroyed with NO backup —
/// precisely the month in which having one matters most. The scope set makes
/// remaining life irrelevant, with no marker to have been written first: this
/// fixture is exactly what `write_session_token` leaves on disk, which is
/// exactly what every pre-upgrade install holds.
#[test]
fn a_mint_in_its_final_month_is_still_preserved() {
    let _home = HomeSandbox::new();
    let name = "kind-late-mint";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;

    // A genuine mint stamped only 10 days out: well inside the 30-day horizon
    // the old inference used to disqualify it.
    let mint = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-a-mint-in-its-final-month-1234".to_string(),
            refresh_token: None,
            expires_at: Some(now + 10 * 86_400_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&mint).expect("ser"),
    )
    .expect("write mint");

    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolling".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("roll");

    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(dir.join("session-token.static.json"))
            .expect("the mint must have been preserved"),
    )
    .expect("parse");
    assert_eq!(
        backup.access_token(),
        Some("sk-ant-oat01-a-mint-in-its-final-month-1234"),
        "a mint's remaining life must not decide whether it is worth keeping"
    );
    assert!(
        matches!(
            sidecar_summary(&crate::profile::ProfileName::from(name)),
            Some((SidecarKind::Rolling, _))
        ),
        "and the sidecar now classifies as the rolling bearer it holds"
    );
}

/// The other side of the same inference: a rolling bearer must never be
/// snapshotted as "the mint", or a later restore installs a token that died
/// hours after it was taken. Two fixtures, because the second is the one the
/// old horizon heuristic got WRONG: a rolling bearer whose stamped expiry is
/// year-scale (a hand-edited clock, or a chain with an anomalous expiry) reads
/// mint-shaped on every axis except its scope set.
#[test]
fn a_rolling_bearer_is_never_preserved_as_the_mint() {
    let _home = HomeSandbox::new();
    let name = "kind-roll-shape";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;

    let chain_scoped = |at: &str, exp: i64, plan: Option<&str>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: at.to_string(),
            refresh_token: None,
            expires_at: Some(exp),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: plan.map(String::from),
        }),
    };

    // What a roll actually writes: hours-scale, plan-stamped, chain-scoped.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&chain_scoped(
            "at-rolling",
            now + 8 * 3_600_000,
            Some("max"),
        ))
        .expect("ser"),
    )
    .expect("write");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-next".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("roll");
    assert!(
        !dir.join("session-token.static.json").exists(),
        "a rolling bearer must never become the degrade backup"
    );

    // The horizon heuristic's blind spot: year-scale expiry, no plan stamp —
    // only the chain scopes give it away. Under the old inference this was
    // snapshotted as "the mint", permanently, since a backup is never replaced.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&chain_scoped(
            "at-far-rolling",
            now + 300 * 86_400_000,
            None,
        ))
        .expect("ser"),
    )
    .expect("write");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-next-2".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("roll 2");
    assert!(
        !dir.join("session-token.static.json").exists(),
        "a far-future stamp must not launder a chain-scoped bearer into the backup"
    );
}

/// Quarantine (the CLI pre-clear) REMOVES a mis-filled sidecar after copying
/// the evidence under the profile.
#[test]
fn quarantining_a_misfill_removes_the_sidecar() {
    let _home = HomeSandbox::new();
    let name = "kind-quarantine";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let misfill = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at".to_string(),
            refresh_token: Some("rt-present".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&misfill).expect("ser"),
    )
    .expect("write");
    assert!(
        quarantine_misfilled_sidecar(&crate::profile::ProfileName::from(name)).expect("quarantine")
    );
    assert!(!dir.join("session-token.json").exists(), "sidecar removed");
    assert!(
        dir.join("quarantine").exists(),
        "evidence kept under the profile"
    );
}

/// `arm_rolling_from_disk` used to stamp the sidecar with the token it read
/// BEFORE taking the rotation guard. `RotationGuard::acquire` BLOCKS, so the
/// ordinary interleaving is: wait for the daemon's rotation to land, then write
/// the value that rotation just superseded. Whether a refresh invalidates its
/// predecessor or leaves it alive to its own `exp`, the outcome is wrong the
/// same way — the session gets a bearer with less life than it should have, or
/// a dead one with no refresh path behind it.
///
/// Reproduced for real: this thread HOLDS the guard while the arming thread is
/// blocked inside `acquire`, advances the stored chain underneath it, and only
/// then releases. What lands in the sidecar must be the post-guard read.
///
/// The interleaving is EXACT, not slept into: the armer runs through the
/// `arm_rolling_from_disk_synced` seam, whose closure fires after the
/// pre-guard read and immediately before `acquire` — so by the time this
/// thread passes the barrier and advances the chain, the pre-guard snapshot
/// provably holds the superseded value. Without the barrier, a pre-guard read
/// landing after the advance would pass against unfixed code too.
#[test]
fn arming_from_disk_stamps_the_post_guard_chain_not_a_stale_snapshot() {
    let _home = HomeSandbox::new();
    let name = "arm-postguard";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;

    let chain = |at: &str, rt: &str, life_h: i64| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: at.to_string(),
            refresh_token: Some(rt.to_string()),
            expires_at: Some(now + life_h * 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        }),
    };

    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.rolling_token = true;
    profile.credentials = Some(chain("at-superseded", "rt-old", 8));
    crate::profile::save_profile(&profile).expect("save");

    // A sidecar inside the arming grace, so the leg has work to do.
    let stale = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-stale-sidecar".to_string(),
            refresh_token: None,
            expires_at: Some(now + 60_000),
            scopes: None,
            subscription_type: Some("max".into()),
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&stale).expect("ser"),
    )
    .expect("write sidecar");

    // Stand in for the daemon's in-flight rotation: hold the guard, let the
    // arming leg finish its pre-guard read and park on the flock, move the
    // chain, then release.
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("hold the guard");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let armer_barrier = std::sync::Arc::clone(&barrier);
    let armer = std::thread::spawn(move || {
        arm_rolling_from_disk_synced(&crate::profile::ProfileName::from("arm-postguard"), || {
            armer_barrier.wait();
        })
    });
    // Past this wait the armer has PROVABLY taken its pre-guard snapshot and
    // is headed into `acquire`, where the held guard parks it.
    barrier.wait();
    profile.credentials = Some(chain("at-rotated", "rt-new", 9));
    crate::profile::save_profile(&profile).expect("save rotated");
    drop(guard);
    armer.join().expect("arming thread");

    let creds: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
            .expect("parse");
    assert_eq!(
        creds.access_token(),
        Some("at-rotated"),
        "the sidecar must carry the POST-guard chain, never the pre-guard snapshot"
    );
    assert!(
        creds.refresh_token().is_none(),
        "nothing rotatable ever reaches the sidecar"
    );
}

/// The post-guard re-read covers the FLAG, not just the chain: a
/// `static-token --clear` can hold this same guard, disarm the profile, take
/// the sidecar and the preserved mint, and release — all while the arming leg
/// parks. Stamping from the pre-guard routing would land a fresh rolling
/// bearer on the profile the operator just cleared, with the flag now off so
/// nothing ever re-stamps it: a dies-in-hours credential with no exit. Same
/// Barrier seam as the chain test above, so the interleaving is pinned by
/// construction.
#[test]
fn arming_from_disk_skips_a_profile_cleared_while_it_waited() {
    let _home = HomeSandbox::new();
    let name = "arm-cleared";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;

    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.rolling_token = true;
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-comfortable".to_string(),
            refresh_token: Some("rt-live".to_string()),
            expires_at: Some(now + 8 * 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        }),
    });
    crate::profile::save_profile(&profile).expect("save");

    // A stale sidecar, so the pre-guard pre-filter sees work to do.
    let stale = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-stale-sidecar".to_string(),
            refresh_token: None,
            expires_at: Some(now + 60_000),
            scopes: None,
            subscription_type: Some("max".into()),
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&stale).expect("ser"),
    )
    .expect("write sidecar");

    // Stand in for the clear: hold the guard, let the arming leg pass its
    // pre-filter and park, then disarm the flag and take the sidecar — the
    // clear's own writes — and release.
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("hold the guard");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let armer_barrier = std::sync::Arc::clone(&barrier);
    let armer = std::thread::spawn(move || {
        arm_rolling_from_disk_synced(&crate::profile::ProfileName::from("arm-cleared"), || {
            armer_barrier.wait();
        })
    });
    barrier.wait();
    profile.rolling_token = false;
    crate::profile::save_profile(&profile).expect("disarm");
    std::fs::remove_file(dir.join("session-token.json")).expect("take the sidecar");
    drop(guard);
    armer.join().expect("arming thread");

    assert!(
        !dir.join("session-token.json").exists(),
        "a cleared profile stays cleared — the pre-guard routing must not re-create the sidecar"
    );
}

/// The restore paths CONSUME the backup, which under the marker design made
/// them the one place a stale claim could destroy the only copy. With
/// classification derived from content, the property is structural — a
/// restored mint classifies as a mint because it IS one — but the end-to-end
/// consequence still deserves its own pin: restore over a rolling sidecar,
/// roll again, and the mint must be back in the backup rather than gone.
#[test]
fn a_restored_mint_is_preserved_again_on_the_next_roll() {
    let _home = HomeSandbox::new();
    let name = "kind-restore-reroll";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;

    let mint = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-the-preserved-mint-111111111".to_string(),
            refresh_token: None,
            expires_at: Some(now + 300 * 86_400_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&mint).expect("ser"),
    )
    .expect("write backup");
    std::fs::write(dir.join("session-token.json"), b"{}").expect("write sidecar");

    assert!(restore_static_mint(&crate::profile::ProfileName::from(name)).expect("restore"));
    assert!(
        matches!(
            sidecar_summary(&crate::profile::ProfileName::from(name)),
            Some((SidecarKind::Mint, _))
        ),
        "a restored mint classifies as the mint it is"
    );
    assert!(
        !dir.join("session-token.static.json").exists(),
        "backup consumed"
    );

    // The next roll must put it straight back — the degrade ladder's rung is
    // rebuilt, not spent.
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolling".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("re-roll");
    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(dir.join("session-token.static.json"))
            .expect("the restored mint must be preserved again"),
    )
    .expect("parse");
    assert_eq!(
        backup.access_token(),
        Some("sk-ant-oat01-the-preserved-mint-111111111")
    );
}

/// The chain-staleness gate is re-applied AFTER the guard wait, against a
/// re-taken clock: the pre-guard pass only proved a comfortable chain existed
/// THEN, and `acquire` can block for a full rotation round trip. If the chain
/// re-read under the guard is already inside the arming grace, stamping it
/// would install a bearer with less life than a session can rely on — the
/// guarded refresh owns that case, so the armer must write nothing.
#[test]
fn arming_from_disk_rechecks_chain_staleness_after_the_guard_wait() {
    let _home = HomeSandbox::new();
    let name = "arm-restale";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;

    let chain = |at: &str, life_min: i64| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: at.to_string(),
            refresh_token: Some("rt".to_string()),
            expires_at: Some(now + life_min * 60_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        }),
    };

    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.rolling_token = true;
    // Comfortable at the pre-guard read: 8h of life.
    profile.credentials = Some(chain("at-was-comfortable", 8 * 60));
    crate::profile::save_profile(&profile).expect("save");

    // A sidecar inside the arming grace, so the leg has work to do.
    let stale = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-stale-sidecar".to_string(),
            refresh_token: None,
            expires_at: Some(now + 60_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&stale).expect("ser"),
    )
    .expect("write sidecar");

    // While the armer is parked on the guard, the chain goes stale: the value
    // it will re-read post-guard has only 10 minutes left — inside the grace.
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("hold the guard");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let armer_barrier = std::sync::Arc::clone(&barrier);
    let armer = std::thread::spawn(move || {
        arm_rolling_from_disk_synced(&crate::profile::ProfileName::from("arm-restale"), || {
            armer_barrier.wait();
        })
    });
    barrier.wait();
    profile.credentials = Some(chain("at-now-stale", 10));
    crate::profile::save_profile(&profile).expect("save stale");
    drop(guard);
    armer.join().expect("arming thread");

    let creds: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
            .expect("parse");
    assert_eq!(
        creds.access_token(),
        Some("at-stale-sidecar"),
        "a chain that went stale during the guard wait must not be stamped"
    );
}

/// A mis-fill classifies as ITSELF, never as the rolling bearer its
/// chain-shaped scopes would suggest — the refresh token is a content fact
/// that pre-empts the scope inference. This is the arm `status.json` rides
/// on: without it a mis-fill published `rolling_token: true` while the TUI
/// rendered `[ mis-filled ]` on the same file, same frame.
#[test]
fn a_rotating_pair_classifies_misfilled_never_rolling() {
    let with_refresh = OAuthToken {
        access_token: "at".to_string(),
        refresh_token: Some("rt".to_string()),
        expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
        scopes: Some(vec![
            "user:inference".to_string(),
            "user:profile".to_string(),
        ]),
        subscription_type: Some("max".into()),
    };
    assert_eq!(sidecar_kind_of(&with_refresh), SidecarKind::Misfilled);
    // And a mis-fill is never preserved as the mint, through the classifier
    // rather than a sibling check.
    let _home = HomeSandbox::new();
    let name = "misfill-preserve";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&ClaudeCredentials {
            claude_ai_oauth: Some(with_refresh),
        })
        .expect("ser"),
    )
    .expect("write");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-next".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("roll");
    assert!(
        !dir.join("session-token.static.json").exists(),
        "a rotating pair must never become the degrade backup"
    );
}

/// The backup slot's one shared rule ([`classify_backup_bytes`]): bytes that
/// are not a genuine mint never restore, from ANY consumer. The shape that
/// motivated it — a parseable file with no `claudeAiOauth` block — used to
/// split the pair: `preserve_static_mint` read it as dead (replace) while the
/// restore path read it as live, installed it, `has_session_token` went
/// false, and sessions got the rotating pair. Not-a-mint content is
/// quarantined (evidence, same as a mis-filled sidecar) and the slot cleared,
/// so it also cannot trap `clauth static-token` in a loop its own recovery
/// advice cannot exit.
#[test]
fn a_backup_that_is_not_a_mint_is_quarantined_never_restored() {
    let _home = HomeSandbox::new();
    let name = "nonmint-backup";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    // A live rolling bearer in the sidecar — what a bad restore would destroy.
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-alive".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("stamp");
    for (label, bytes) in [
        (
            "blockless",
            serde_json::to_vec(&ClaudeCredentials {
                claude_ai_oauth: None,
            })
            .expect("ser"),
        ),
        (
            "rotating pair",
            serde_json::to_vec(&creds("at-pair", Some("rt-pair"))).expect("ser"),
        ),
    ] {
        std::fs::write(dir.join("session-token.static.json"), &bytes).expect("write backup");
        assert!(
            !restore_static_mint(&crate::profile::ProfileName::from(name))
                .expect("restore verdict"),
            "a {label} backup restores nothing"
        );
        assert!(
            !dir.join("session-token.static.json").exists(),
            "the {label} slot-holder is quarantined away, not left in place"
        );
        let sidecar: ClaudeCredentials =
            serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
                .expect("parse");
        assert_eq!(
            sidecar.access_token(),
            Some("at-alive"),
            "the live bearer is untouched by a refused {label} restore"
        );
    }
    let quarantined = std::fs::read_dir(dir.join("quarantine"))
        .expect("quarantine dir")
        .filter(|e| {
            e.as_ref().is_ok_and(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".session-token.static.json")
            })
        })
        .count();
    assert_eq!(
        quarantined, 2,
        "both refused slot-holders survive as evidence"
    );
}

/// An EXPIRED backup is refused by every restore path and left on disk:
/// installing it would sign sessions out on first use, and consuming it would
/// also destroy whatever life the current sidecar has left.
#[test]
fn an_expired_backup_is_never_restored() {
    let _home = HomeSandbox::new();
    let name = "expired-backup";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    let expired = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-aged-out".to_string(),
            refresh_token: None,
            expires_at: Some(now - 86_400_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&expired).expect("ser"),
    )
    .expect("write backup");
    let live_bearer = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-still-alive".to_string(),
            refresh_token: None,
            expires_at: Some(now + 2 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&live_bearer).expect("ser"),
    )
    .expect("write sidecar");

    assert!(
        !restore_static_mint(&crate::profile::ProfileName::from(name)).expect("restore"),
        "an expired backup reads as nothing-to-restore"
    );
    assert!(
        dir.join("session-token.static.json").exists(),
        "and stays on disk as evidence"
    );
    let sidecar_now: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
            .expect("parse");
    assert_eq!(
        sidecar_now.access_token(),
        Some("at-still-alive"),
        "the sidecar's remaining live bearer is not destroyed for a dead mint"
    );

    // The heal path refuses the same way: a mis-fill plus an expired backup
    // keeps the disengaged-but-working posture instead of installing a dead
    // credential over it.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("at-misfill", Some("rt-misfill"))).expect("ser"),
    )
    .expect("misfill");
    assert_eq!(
        heal_misfilled_sidecar(&crate::profile::ProfileName::from(name)).expect("heal"),
        HealOutcome::NoLiveBackup
    );
    assert!(
        matches!(
            session_token_status(&crate::profile::ProfileName::from(name)),
            Some(SessionTokenStatus::NotLongLived)
        ),
        "the mis-fill stays in place rather than trading working-vanilla for a dead mint"
    );
    assert!(dir.join("session-token.static.json").exists());
}

/// One transient read failure on the sidecar must ABORT the roll, not forfeit
/// the mint: `preserve_static_mint`'s verdict decides whether the stamp may
/// overwrite the file, and a swallowed `EISDIR`/`EIO` there would destroy a
/// genuine mint with no backup written — the one unrecoverable direction.
#[test]
fn an_unreadable_sidecar_aborts_the_roll_instead_of_forfeiting_the_mint() {
    let _home = HomeSandbox::new();
    let name = "unreadable-sidecar";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A directory where the sidecar goes: reads fail with a non-NotFound
    // error on every platform, which is the shape of a transient fault.
    std::fs::create_dir(dir.join("session-token.json")).expect("block the sidecar path");
    let err = stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-next".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    );
    let err =
        err.expect_err("a roll that cannot read what it is about to overwrite must not proceed");
    // Pinned to the READ arm's context, not just "some error": a directory at
    // the sidecar path also fails the WRITE, so an any-error assert passed
    // against the unfixed code (verification fleet, round 3).
    assert!(
        format!("{err:#}").contains("before overwriting"),
        "the failure must come from the pre-overwrite read, not the later write: {err:#}"
    );
    assert!(
        !dir.join("session-token.static.json").exists(),
        "and no half-truth backup appears"
    );
}

/// An expired backup does not poison the slot: the next preserve REPLACES it
/// with the fresh mint being superseded. Left in place behind the bare
/// `exists()` idempotence guard, the dead file blocked every future mint from
/// preservation — re-mint, re-arm, and the fresh mint was destroyed on the
/// next roll with only the dead backup left to restore.
#[test]
fn a_fresh_mint_replaces_an_expired_backup_on_the_next_roll() {
    let _home = HomeSandbox::new();
    let name = "expired-backup-replaced";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    let expired = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-dead-m1".to_string(),
            refresh_token: None,
            expires_at: Some(now - 86_400_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&expired).expect("ser"),
    )
    .expect("write dead backup");
    // The fresh mint the operator just captured (flag was off, so no
    // with-backup write happened).
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-fresh-m2",
        now,
    )
    .expect("mint");

    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolled".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("roll");

    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(dir.join("session-token.static.json")).expect("read backup"),
    )
    .expect("parse");
    assert_eq!(
        backup.access_token(),
        Some("sk-ant-oat01-fresh-m2"),
        "the fresh mint displaces the dead backup instead of dying behind it"
    );
}

/// A LIVE backup is not enough to stand — it must also be at least as FRESH
/// as the mint the roll is about to overwrite. The subtler variant of the
/// dead-slot failure: flag off, `clauth login --setup-token` writes the
/// sidecar alone, and "an existing backup is never replaced" let the next
/// roll destroy the fresh year-scale mint while preserving a stale
/// weeks-from-death one. A genuinely staler sidecar mint must NOT displace a
/// fresher backup, and repeated rolls (rolling content in the sidecar) must
/// touch nothing — the idempotence that made the old rule attractive, kept.
#[test]
fn a_fresher_sidecar_mint_upgrades_a_live_but_older_backup() {
    let _home = HomeSandbox::new();
    let name = "older-backup-upgraded";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    let mint = |token: &str, exp: i64| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: token.to_string(),
            refresh_token: None,
            expires_at: Some(exp),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    let roll = || {
        stamp_rolling_token(
            &crate::profile::ProfileName::from(name),
            &OAuthToken {
                access_token: "at-rolled".to_string(),
                refresh_token: None,
                expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:profile".to_string(),
                ]),
                subscription_type: Some("max".into()),
            },
        )
        .expect("roll");
    };
    // Live but three days from death — the stale holder.
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&mint("sk-ant-oat01-stale-m1", now + 3 * 86_400_000))
            .expect("ser"),
    )
    .expect("write stale backup");
    // The fresh re-mint sits only in the sidecar (flag was off).
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-fresh-m2",
        now,
    )
    .expect("mint");
    roll();
    let read_backup = || -> ClaudeCredentials {
        serde_json::from_slice(
            &std::fs::read(dir.join("session-token.static.json")).expect("read backup"),
        )
        .expect("parse")
    };
    assert_eq!(
        read_backup().access_token(),
        Some("sk-ant-oat01-fresh-m2"),
        "the fresher mint displaces the live-but-older backup"
    );
    // A second roll sees rolling content in the sidecar: nothing to preserve,
    // the upgraded backup stands.
    roll();
    assert_eq!(read_backup().access_token(), Some("sk-ant-oat01-fresh-m2"));
    // And the comparison is not "sidecar always wins": a STALER mint placed
    // in the sidecar leaves a fresher live backup exactly where it is.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&mint("sk-ant-oat01-staler-m3", now + 86_400_000)).expect("ser"),
    )
    .expect("write staler sidecar mint");
    roll();
    assert_eq!(
        read_backup().access_token(),
        Some("sk-ant-oat01-fresh-m2"),
        "a staler sidecar mint never displaces a fresher live backup"
    );
}

/// The displaced-holder disposal rule matches `live_backup_bytes`: a slot
/// holder that never was a mint is quarantined as evidence before the mint
/// replaces it, while a dead mint is just a superseded credential and is
/// overwritten in place (the sibling tests above pin that half).
#[test]
fn preserve_quarantines_a_displaced_slot_holder_that_was_never_a_mint() {
    let _home = HomeSandbox::new();
    let name = "nonmint-slot-displaced";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec(&creds("at-pair", Some("rt-pair"))).expect("ser"),
    )
    .expect("write pair into the slot");
    write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-genuine",
        now,
    )
    .expect("mint");
    stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolled".to_string(),
            refresh_token: None,
            expires_at: Some(now + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("roll");
    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(dir.join("session-token.static.json")).expect("read backup"),
    )
    .expect("parse");
    assert_eq!(
        backup.access_token(),
        Some("sk-ant-oat01-genuine"),
        "the mint takes the slot"
    );
    let quarantined = std::fs::read_dir(dir.join("quarantine"))
        .expect("quarantine dir")
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with(".session-token.static.json")
        });
    assert!(quarantined, "the displaced pair survives as evidence");
}

/// A backup inside Claude Code's own five-minute refresh window reads as
/// expired: CC refreshes a credential once it is inside five minutes of
/// expiry, and a refresh-less mint cannot answer that — restoring one
/// consumes the backup only to sign the session out moments later. Pins the
/// WINDOW itself, not just past-expiry (a zero grace leaves past-expiry
/// refusals intact and this test red).
#[test]
fn a_backup_inside_ccs_refresh_window_reads_as_expired() {
    let _home = HomeSandbox::new();
    let name = "window-backup";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    let closing = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-two-minutes-left".to_string(),
            refresh_token: None,
            expires_at: Some(now + 2 * 60 * 1000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&closing).expect("ser"),
    )
    .expect("write closing backup");
    assert!(
        !restore_static_mint(&crate::profile::ProfileName::from(name)).expect("restore verdict"),
        "two minutes of life is dead-on-arrival for a refresh-less mint"
    );
    assert!(
        dir.join("session-token.static.json").exists(),
        "refused like any expired mint: left in place, never quarantined"
    );
}

/// `restore_static_mint` overwrites the sidecar — and when the sidecar holds
/// a rotating pair (a mis-fill), that content is EVIDENCE, quarantined
/// exactly as the heal and CLI pre-clear paths quarantine it. This was the
/// one repair that destroyed the pair silently.
#[test]
fn restore_quarantines_a_misfilled_sidecar_before_overwriting_it() {
    let _home = HomeSandbox::new();
    let name = "restore-misfill-evidence";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let now = crate::usage::now_ms() as i64;
    let live_mint = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-preserved".to_string(),
            refresh_token: None,
            expires_at: Some(now + 180 * 86_400_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&live_mint).expect("ser"),
    )
    .expect("write backup");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&creds("at-misfill", Some("rt-misfill"))).expect("ser"),
    )
    .expect("misfill");
    assert!(restore_static_mint(&crate::profile::ProfileName::from(name)).expect("restore"));
    let sidecar: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
            .expect("parse");
    assert_eq!(sidecar.access_token(), Some("sk-ant-oat01-preserved"));
    let quarantined = std::fs::read_dir(dir.join("quarantine"))
        .expect("quarantine dir")
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with(".session-token.json")
        });
    assert!(
        quarantined,
        "the overwritten pair survives as evidence, same as every other repair path"
    );
}

// ── stale-config persist gate (lock-race row 3) ───────────────────────────────

/// The force-capture sink writes a whole profile, so it must not recreate the
/// directory of an active profile deleted by the CLI while a stale config held
/// the pre-delete snapshot. The live mirror file stays (the delete runs on a
/// config where the profile is not active), so the sink reaches its capture
/// branch rather than the absent-file skip.
#[test]
fn force_snapshot_does_not_resurrect_a_deleted_profile() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("gone".to_string(), None, None);
    profile.credentials = Some(creds("stored-access", Some("stored-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let mut config = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("gone".into());
    config.state.profiles = vec!["gone".into()];
    crate::profile::save_app_state(&config.state).expect("persist state");

    // The leg's config is a snapshot taken BEFORE the delete.
    let mut stale = config.clone();

    let live = claude_credentials_path().expect("creds path");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live,
        serde_json::to_vec(&creds("relogin-access", Some("relogin-refresh"))).expect("ser"),
    )
    .expect("write live");

    // CLI account mutation on a config where `gone` is NOT active, so the delete
    // leaves the live file alone.
    let mut disk = crate::profile::load_config().expect("load disk config");
    disk.state.active_profile = None;
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("gone"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("gone"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);
    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from("gone"))
            .expect("dir")
            .exists(),
        "fixture precondition: the delete removed the directory"
    );

    force_snapshot_active_credentials(&mut stale).expect("force snapshot");

    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from("gone"))
            .expect("dir")
            .exists(),
        "a deleted profile's directory must not be resurrected by the capture sink"
    );
}

// ---------------------------------------------------------------------------
// Staged credential publishes. Every credential link is replaced by a rename
// over a staging sibling, so the live path is never absent between two calls
// and no staging file outlives the swap.
// ---------------------------------------------------------------------------

/// Names in `dir` that every tree walk skips by name (`watchdog::is_staging`).
/// One left behind is invisible to the mirror, so nothing ever collects it.
#[cfg(unix)]
fn staging_siblings(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.') && n.contains(".tmp."))
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
#[test]
fn a_credential_publish_repoints_the_live_link_and_strands_no_staging_file() {
    let _home = HomeSandbox::new();
    for name in ["from", "onto"] {
        let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
        profile.credentials = Some(creds(&format!("{name}-access"), Some("refresh")));
        crate::profile::save_profile(&profile).expect("save profile");
    }
    let store = |name: &str| {
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("dir")
            .join("credentials.json")
    };

    let live = claude_credentials_path().expect("creds path");
    let claude_home = live.parent().expect("parent").to_path_buf();
    fs::create_dir_all(&claude_home).expect("mkdir .claude");
    std::os::unix::fs::symlink(store("from"), &live).expect("seed the outgoing link");

    link_profile_credentials(&crate::profile::ProfileName::from("onto")).expect("publish");

    assert_eq!(
        fs::read_link(&live).expect("the live path is still a link"),
        store("onto"),
        "the publish repointed the live link at the incoming profile's store"
    );
    assert_eq!(
        staging_siblings(&claude_home),
        Vec::<String>::new(),
        "the staging sibling was renamed away rather than left in ~/.claude"
    );
}

/// A publish that cannot land leaves the destination exactly as it was and
/// takes its staging file with it. The unlink-then-create this replaced removed
/// the live path FIRST, so the same failure left it with nothing in it.
#[cfg(unix)]
#[test]
fn a_failed_credential_publish_keeps_the_destination_and_strands_nothing() {
    let _home = HomeSandbox::new();
    let mut profile = crate::profile::Profile::new("onto".to_string(), None, None);
    profile.credentials = Some(creds("onto-access", Some("refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    let live = claude_credentials_path().expect("creds path");
    let claude_home = live.parent().expect("parent").to_path_buf();
    // A directory at the live path refuses the rename on every platform, which
    // fails the swap itself rather than its staging — the half under test.
    fs::create_dir_all(&live).expect("mkdir a destination no rename can replace");

    let err = force_link_profile_credentials(&crate::profile::ProfileName::from("onto"))
        .expect_err("the publish must fail");
    assert!(
        format!("{err:#}").contains("failed to publish"),
        "the error names the publish that failed: {err:#}"
    );
    assert!(
        live.is_dir(),
        "a failed publish leaves the destination untouched"
    );
    assert_eq!(
        staging_siblings(&claude_home),
        Vec::<String>::new(),
        "the staging file is cleaned up on the failure arm"
    );
}
