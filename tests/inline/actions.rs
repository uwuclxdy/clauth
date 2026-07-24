#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Per-account custom env editor: collision classification + `edit_profile_env`
//! persistence and the strip-removed-keys-on-active behaviour.

use super::*;
use crate::profile::AppState;
use crate::testutil::HomeSandbox;

fn acct_config() -> AppConfig {
    AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("acct".to_string(), None, None)],
    }
}

#[test]
fn classify_env_key_flags_managed_keys() {
    let p = Profile::new("acct".to_string(), None, None);
    assert!(matches!(
        classify_env_key(&p, &[], "ANTHROPIC_BASE_URL"),
        Some(EnvKeyCollision::Managed(_))
    ));
    assert!(matches!(
        classify_env_key(&p, &[], "CLAUDE_CODE_SUBAGENT_MODEL"),
        Some(EnvKeyCollision::Managed(_))
    ));
    assert_eq!(classify_env_key(&p, &[], "ANTHROPIC_CUSTOM_FLAG"), None);
}

#[test]
fn classify_env_key_flags_own_field_by_sorted_index() {
    let mut p = Profile::new("acct".to_string(), None, None);
    p.env.insert("ZED".to_string(), "1".to_string());
    p.env.insert("ALPHA".to_string(), "2".to_string());
    // BTreeMap order: ALPHA(0), ZED(1).
    assert_eq!(
        classify_env_key(&p, &[], "ALPHA"),
        Some(EnvKeyCollision::ProfileField(0))
    );
    assert_eq!(
        classify_env_key(&p, &[], "ZED"),
        Some(EnvKeyCollision::ProfileField(1))
    );
}

#[test]
fn classify_env_key_base_settings_only_for_external_keys() {
    let mut p = Profile::new("acct".to_string(), None, None);
    p.env.insert("OWN".to_string(), "1".to_string());
    let base = vec![
        "OWN".to_string(),
        "EXTERNAL".to_string(),
        "ANTHROPIC_BASE_URL".to_string(),
    ];
    // Managed + own-field checks win before the base check, so only a key that is
    // neither (genuinely external) classifies as BaseSettings.
    assert_eq!(
        classify_env_key(&p, &base, "EXTERNAL"),
        Some(EnvKeyCollision::BaseSettings)
    );
    assert_eq!(
        classify_env_key(&p, &base, "OWN"),
        Some(EnvKeyCollision::ProfileField(0))
    );
    assert!(matches!(
        classify_env_key(&p, &base, "ANTHROPIC_BASE_URL"),
        Some(EnvKeyCollision::Managed(_))
    ));
    assert_eq!(classify_env_key(&p, &base, "FRESH"), None);
}

/// macOS reality: `~/.claude/.credentials.json` is a regular-file Keychain mirror
/// of the ACTIVE account (not clauth's symlink). Switching to another profile must
/// succeed — the live file matches the active profile (already captured), so it is
/// safe to replace even though it legitimately differs from the target. Regression
/// for `Error: refusing to replace .credentials.json — live file differs from
/// profile 'xfx'; resolve divergence first` on every `clauth <name>`.
#[test]
fn switch_replaces_active_account_mirror_without_refusing() {
    let _home = HomeSandbox::new();

    let mk = |name: &str, access: &str| {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    let active = mk("cl-ax", "cl-ax-access");
    let target = mk("xfx", "xfx-access");

    // Live file = a plain regular file whose content matches the ACTIVE profile
    // (exactly what Claude Code mirrors from the Keychain on macOS).
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(active.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![active, target],
    };
    config.state.active_profile = Some("cl-ax".into());

    // Must NOT bail — the live file is the active account's captured mirror.
    switch_profile(&mut config, "xfx").expect("switch replaces the active-account mirror");

    assert!(config.is_active("xfx"));
    assert_eq!(
        crate::claude::classify_credentials_link("xfx").expect("classify"),
        crate::claude::LinkState::LinkedTo,
        "after the switch the live path resolves to xfx's stored creds",
    );
}

/// `switch_profile` to a name with no profile must bail BEFORE any side
/// effect. Pre-fix the existence check lived in `finish_switch` — LAST in the
/// sequence — so `force_link_profile_credentials` had already torn down the
/// live `.credentials.json` for a ghost target (a deleted profile still queued
/// in a daemon's pending-switch), and a retry then misread the missing live
/// file as "logged out", nulling the ACTIVE profile's stored credentials
/// (2026-07-12 review, HIGH).
#[test]
fn switch_to_a_missing_profile_bails_before_touching_the_live_link() {
    let _home = HomeSandbox::new();

    let mut p = Profile::new("keeper".to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "keeper-access".to_string(),
            refresh_token: Some("keeper-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(p.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![p],
    };
    config.state.active_profile = Some("keeper".into());

    let err = switch_profile(&mut config, "ghost").expect_err("ghost must bail");
    assert!(
        err.to_string().contains("not found"),
        "bail names the cause, got: {err}"
    );
    assert!(config.is_active("keeper"), "active unchanged");
    assert!(live_path.exists(), "the live credentials file survives");
    let stored = crate::profile::profile_dir("keeper")
        .unwrap()
        .join("credentials.json");
    assert!(stored.exists(), "keeper's stored credentials survive");
}

/// The disabled gate is the shared locked action, not a CLI wrapper:
/// `switch_profile` itself — no `cmd_switch`, no MCP tool — refuses a
/// disabled target and leaves `active_profile` untouched. Covers #1/#4/#6:
/// every switch primitive funnels through the same `ensure_switch_target_ok`
/// chokepoint this exercises directly.
#[test]
fn switch_profile_refuses_a_disabled_target_and_leaves_active_unchanged() {
    let _home = HomeSandbox::new();
    let active = Profile::new("active".to_string(), None, None);
    save_profile(&active).expect("save active");
    let mut target = Profile::new("target".to_string(), None, None);
    target.disabled = true;

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..AppState::default()
        },
        profiles: vec![active, target],
    };

    let err = switch_profile(&mut config, "target").expect_err("a disabled target must be refused");
    assert_eq!(
        err.to_string(),
        "'target': account is disabled, run `clauth enable target`"
    );
    assert!(
        config.is_active("active"),
        "active profile must be unchanged"
    );
}

/// AUTH-4 parity, TUI side: `auto_switch_if_needed` must leave an auth-broken
/// active even when its (frozen-stale) usage still reads as headroom — the
/// same wedge `scan_auto_switch` had on the daemon side. Pre-fix, the
/// exhaustion gate alone returned `None` here and the TUI parked on the dead
/// account forever.
#[test]
fn auto_switch_if_needed_walks_off_a_broken_active() {
    use crate::fallback::{SwitchAction, auto_switch_if_needed};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let _home = HomeSandbox::new();

    let mk = |name: &str, access: &str, util: f64, resets_at: i64| {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        p.usage = Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: Some(epoch_secs_to_iso(resets_at)),
            }),
            ..Default::default()
        });
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    // Active "a": broken, last-ever read maxed on a LAPSED window (reads as
    // idle headroom). Target "b": healthy, live window with real headroom.
    let a = mk("a", "a-access", 100.0, now_epoch_secs() - 3600);
    let b = mk("b", "b-access", 10.0, now_epoch_secs() + 3600);

    // Live file = the active account's own captured mirror (macOS shape), so
    // the switch's foreign-file guard sees its own mirror and proceeds.
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(a.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into()],
            fallback_chain: vec!["a".into(), "b".into()],
            auth_broken: vec!["a".into()],
            ..AppState::default()
        },
        profiles: vec![a, b],
    };

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(
        action,
        Some(SwitchAction::To("b".to_string())),
        "a dead active with stale-headroom usage must still be walked away from"
    );
    assert!(config.is_active("b"));
}

/// The scoped trigger through the REAL UI one-shot: `auto_switch_if_needed`
/// must hop off an otherwise-healthy active whose per-model week is spent —
/// through `fully_clear_target`, its only walk — and actually land the switch.
#[test]
fn auto_switch_if_needed_hops_off_a_scoped_blocked_active() {
    use crate::fallback::{SwitchAction, auto_switch_if_needed};
    use crate::usage::{ScopedWindow, UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let _home = HomeSandbox::new();

    let creds = |name: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let mk = |name: &str, scoped: Vec<ScopedWindow>| {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds(name));
        p.usage = Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            seven_day: Some(UsageWindow {
                utilization: 40.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)),
            }),
            weekly_scoped: scoped,
            ..Default::default()
        });
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    let a = mk(
        "a",
        vec![ScopedWindow {
            label: "7d fable".into(),
            window: UsageWindow {
                utilization: 100.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)),
            },
        }],
    );
    let b = mk("b", vec![]);
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(a.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into()],
            fallback_chain: vec!["a".into(), "b".into()],
            ..AppState::default()
        },
        profiles: vec![a, b],
    };

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(
        action,
        Some(SwitchAction::To("b".to_string())),
        "a healthy active with a spent per-model week must hop to the clear member"
    );
    assert!(config.is_active("b"));
}

/// Twin of the hop-off test above, but the only sibling is canceled: its
/// cached 5h window reads as idle headroom (the exact shape `is_canceled`
/// exists to catch), yet every request against it 403s. `fully_clear_target`
/// — the scoped trigger's only walk — must skip it same as `next_target`
/// does, so the trigger finds nothing and leaves the scoped-blocked active in
/// place rather than relinking onto a dead account.
#[test]
fn auto_switch_if_needed_does_not_hop_a_scoped_blocked_active_onto_a_canceled_member() {
    use crate::fallback::auto_switch_if_needed;
    use crate::usage::{
        PlanInfo, PlanTier, ScopedWindow, UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs,
    };
    let _home = HomeSandbox::new();

    let creds = |name: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let mut a = Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("a"));
    a.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 10.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        seven_day: Some(UsageWindow {
            utilization: 40.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)),
        }),
        weekly_scoped: vec![ScopedWindow {
            label: "7d fable".into(),
            window: UsageWindow {
                utilization: 100.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)),
            },
        }],
        ..Default::default()
    });
    crate::profile::save_profile(&a).expect("save profile");

    let mut b = Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("b"));
    b.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 5.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    });
    crate::profile::save_profile(&b).expect("save profile");

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(a.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into()],
            fallback_chain: vec!["a".into(), "b".into()],
            ..AppState::default()
        },
        profiles: vec![a, b],
    };

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(
        action, None,
        "a scoped-blocked active must not hop onto a canceled member reading idle headroom"
    );
    assert!(
        config.is_active("a"),
        "active must stay put when the only sibling is canceled"
    );
}

/// The pinned-sink guard on the same one-shot: identical shape, but the
/// active is `last_resort` — parked on purpose, so the scoped hop must not
/// un-park it (the scheduler-walk twin is
/// `scoped_active_trigger_stays_parked_on_a_pinned_sink`).
#[test]
fn auto_switch_if_needed_keeps_a_scoped_blocked_sink_parked() {
    use crate::fallback::auto_switch_if_needed;
    use crate::usage::{ScopedWindow, UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let _home = HomeSandbox::new();

    let creds = |name: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let mut a = Profile::new("a".to_string(), None, None);
    a.credentials = Some(creds("a"));
    a.last_resort = true;
    a.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 10.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        weekly_scoped: vec![ScopedWindow {
            label: "7d fable".into(),
            window: UsageWindow {
                utilization: 100.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)),
            },
        }],
        ..Default::default()
    });
    crate::profile::save_profile(&a).expect("save a");
    let mut b = Profile::new("b".to_string(), None, None);
    b.credentials = Some(creds("b"));
    crate::profile::save_profile(&b).expect("save b");
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(a.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into()],
            fallback_chain: vec!["a".into(), "b".into()],
            ..AppState::default()
        },
        profiles: vec![a, b],
    };

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(action, None, "a pinned sink stays parked");
    assert!(config.is_active("a"));
}

#[test]
fn reauth_capture_upserts_existing_profile_and_clears_broken_flag() {
    // AUTH-3 reauth: `clauth login <existing>` captures fresh tokens INTO the existing
    // profile. It must OVERWRITE (not append a duplicate — `config.add` upserts) and
    // clear the auth-broken quarantine so the account is usable again.
    let _home = HomeSandbox::new();

    let oauth = |access: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("{access}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let mut stale = Profile::new("xfx".to_string(), None, None);
    stale.credentials = Some(oauth("stale-access"));
    crate::profile::save_profile(&stale).expect("save profile");

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![stale],
    };
    config.state.profiles.push("xfx".into());
    config.state.active_profile = Some("xfx".into());
    config.set_auth_broken("xfx", true);
    // Persist the active profile so the capture takes the RE-AUTH path (disk already
    // has this profile active), which force-relinks the fresh creds live.
    crate::profile::save_app_state(&config.state).expect("seed disk state");
    assert!(config.is_auth_broken("xfx"), "precondition: quarantined");

    capture_into_profile(
        &mut config,
        "xfx".to_string(),
        CaptureSnapshot {
            credentials: Some(oauth("fresh-access")),
            base_url: None,
            api_key: None,
            identity: crate::actions::CaptureIdentity::LiveLogin,
        },
    )
    .expect("re-auth capture");

    // Upsert, not append: exactly one profile named xfx in BOTH lists.
    assert_eq!(
        config.profiles.iter().filter(|p| p.name == "xfx").count(),
        1,
        "no duplicate Profile after re-auth"
    );
    assert_eq!(
        config
            .state
            .profiles
            .iter()
            .filter(|n| n.as_str() == "xfx")
            .count(),
        1,
        "no duplicate name in state.profiles"
    );
    // Fresh creds landed, quarantine cleared.
    let access = config
        .profiles
        .iter()
        .find(|p| p.name == "xfx")
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .map(|o| o.access_token.as_str());
    assert_eq!(access, Some("fresh-access"), "credentials overwritten");
    assert!(
        !config.is_auth_broken("xfx"),
        "auth-broken quarantine cleared"
    );
}

#[test]
fn reauth_of_the_active_account_force_relinks_the_stale_mirror() {
    // The bug behind "Not logged in · run /login" after a re-auth: `clauth login
    // <active>` captured fresh tokens into the profile + file but NEVER re-linked the
    // live credential (on macOS the Keychain), so a running `claude` kept reading the
    // dead token. The live `.credentials.json` on macOS is a regular-FILE mirror that
    // differs from the fresh tokens, so the non-force link would REFUSE — the fix
    // force-relinks. (Keychain is disabled under test, so this asserts the file/symlink
    // half; the macOS Keychain write rides the same `force_link_profile_credentials`.)
    let _home = HomeSandbox::new();

    let oauth = |access: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("{access}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    // Active profile "xfx" whose STORED creds are already the fresh ones (capture will
    // save fresh into the profile), but whose LIVE mirror is the OLD dead token.
    let mut xfx = Profile::new("xfx".to_string(), None, None);
    xfx.credentials = Some(oauth("old-dead"));
    crate::profile::save_profile(&xfx).expect("save profile");

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![xfx],
    };
    config.state.profiles.push("xfx".into());
    config.state.active_profile = Some("xfx".into());
    crate::profile::save_app_state(&config.state).expect("seed disk state");

    // The live file: a plain regular file holding the OLD token (what CC mirrored from
    // the Keychain) — DIFFERENT from the fresh tokens capture is about to store.
    let live = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live.parent().unwrap()).unwrap();
    std::fs::write(&live, serde_json::to_vec(&oauth("old-dead")).unwrap()).unwrap();

    capture_into_profile(
        &mut config,
        "xfx".to_string(),
        CaptureSnapshot {
            credentials: Some(oauth("fresh-live")),
            base_url: None,
            api_key: None,
            identity: crate::actions::CaptureIdentity::LiveLogin,
        },
    )
    .expect("re-auth capture force-relinks even though the live mirror diverged");

    // The live credential now resolves to xfx's FRESH stored creds — the force-relink
    // replaced the stale mirror (a non-force link would have bailed "live file differs").
    assert_eq!(
        crate::claude::classify_credentials_link("xfx").expect("classify"),
        crate::claude::LinkState::LinkedTo,
        "the live credential is relinked to the re-authed profile",
    );
    let live_bytes = std::fs::read(&live).unwrap();
    let live_creds: crate::profile::ClaudeCredentials =
        serde_json::from_slice(&live_bytes).unwrap();
    assert_eq!(
        live_creds
            .claude_ai_oauth
            .map(|o| o.access_token)
            .as_deref(),
        Some("fresh-live"),
        "the live login carries the fresh token, not the dead one",
    );
}

#[test]
fn edit_profile_env_persists_to_config_toml() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    let mut env = BTreeMap::new();
    env.insert("FOO".to_string(), "bar".to_string());
    edit_profile_env(&mut config, "acct", env).expect("set env");

    assert_eq!(
        config.find("acct").unwrap().env.get("FOO"),
        Some(&"bar".to_string())
    );
    let toml = std::fs::read_to_string(profile_dir("acct").unwrap().join("config.toml"))
        .expect("config.toml written");
    assert!(
        toml.contains("FOO"),
        "custom env key persisted to config.toml"
    );

    // Clearing the map persists too.
    edit_profile_env(&mut config, "acct", BTreeMap::new()).expect("clear env");
    assert!(config.find("acct").unwrap().env.is_empty());
}

#[test]
fn edit_profile_env_strips_removed_keys_from_live_settings_when_active() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    config.state.active_profile = Some("acct".into());

    let mut env = BTreeMap::new();
    env.insert("KEEP".to_string(), "1".to_string());
    env.insert("DROP".to_string(), "2".to_string());
    edit_profile_env(&mut config, "acct", env).expect("write both");
    let live = crate::claude::claude_settings_env_keys().expect("read settings");
    assert!(live.contains(&"KEEP".to_string()) && live.contains(&"DROP".to_string()));

    // Removing DROP must strip it from the live settings.json, not leak it.
    let mut env2 = BTreeMap::new();
    env2.insert("KEEP".to_string(), "1".to_string());
    edit_profile_env(&mut config, "acct", env2).expect("drop one");
    let live = crate::claude::claude_settings_env_keys().expect("read settings");
    assert!(live.contains(&"KEEP".to_string()));
    assert!(
        !live.contains(&"DROP".to_string()),
        "a removed key is stripped from the live settings on re-apply"
    );
}

// ── set_profile_default_model (`clauth login --model`, the create-form row) ──
// (the ensure_login_profile tests were dropped with the fn — `clauth login` now
//  mints tokens via the browser flow and captures a profile, rather than
//  pre-creating a blank one; `--model` is applied to the captured profile.)

#[test]
fn set_profile_default_model_persists_to_config_toml() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    set_profile_default_model(&mut config, "acct", "opus").expect("set model");

    assert_eq!(
        config.find("acct").unwrap().models.default.as_deref(),
        Some("opus")
    );
    let toml = std::fs::read_to_string(profile_dir("acct").unwrap().join("config.toml"))
        .expect("config.toml written");
    assert!(toml.contains("opus"), "model persisted to config.toml");
}

#[test]
fn set_profile_default_model_preserves_alias_overrides() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    edit_profile_model(
        &mut config,
        "acct",
        ModelSettings {
            opus: Some("claude-opus-4-8".to_string()),
            ..ModelSettings::default()
        },
    )
    .expect("seed opus alias");

    set_profile_default_model(&mut config, "acct", "sonnet").expect("set default");

    let profile = config.find("acct").unwrap();
    assert_eq!(profile.models.default.as_deref(), Some("sonnet"));
    assert_eq!(
        profile.models.opus.as_deref(),
        Some("claude-opus-4-8"),
        "setting the default must not clobber an existing alias override"
    );
}

#[test]
fn set_profile_default_model_blank_clears_default() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    set_profile_default_model(&mut config, "acct", "opus").expect("set model");
    set_profile_default_model(&mut config, "acct", "   ").expect("clear model");
    assert!(
        config.find("acct").unwrap().models.default.is_none(),
        "blank input clears the default, mirroring the Setup tab's ⏎ commit"
    );
}

#[test]
fn validate_profile_name_accepts_email_rejects_path_chars() {
    for name in [
        "claude@domain.com",
        "user2@domain.com",
        "claude+work@gmail.com",
    ] {
        assert!(
            validate_profile_name(name, &[], None).is_ok(),
            "{name} rejected"
        );
    }
    // path separators / windows-reserved chars stay blocked so the name can't
    // escape its profiles/<name> directory segment.
    for name in ["a/b", "a\\b", "a:b", ".lead", "a b"] {
        assert!(
            validate_profile_name(name, &[], None).is_err(),
            "{name} accepted"
        );
    }
}

/// Profiles named exactly after a `clauth` subcommand are permanently
/// unreachable by `clauth <name>` (the subcommand dispatch shadows the switch),
/// so creation refuses them — case-insensitively, since a `Daemon` profile
/// would switch while `daemon` runs the daemon.
#[test]
fn validate_profile_name_rejects_reserved_subcommand_names() {
    for name in [
        "daemon",
        "status",
        "doctor",
        "which",
        "start",
        "login",
        "delete",
        "resume",
        "run",
        "mcp",
        "__complete",
        "mcp-await-job",
        "Daemon",
        "STATUS",
        "Doctor",
    ] {
        let err = validate_profile_name(name, &[], None)
            .expect_err("reserved subcommand name must be refused");
        assert!(
            err.to_string().contains("reserved"),
            "{name} rejected for the wrong reason: {err}"
        );
    }
    // `completions` is NOT reserved — bare `clauth completions` falls through to
    // a switch — and ordinary names still pass.
    for name in ["completions", "work", "daemon-2", "my-daemon", "personal"] {
        assert!(
            validate_profile_name(name, &[], None).is_ok(),
            "{name} wrongly rejected"
        );
    }
}

// ── capture-name collision overwrite (issue #7) ────────────────────────────

/// Overwriting an existing profile on a capture-name collision must mutate it
/// in place: chain position, env, model/fallback config, and auto_start
/// survive; only credentials/base_url/api_key change; usage_history.jsonl
/// (a persisted log, not a cache) is untouched; the stale per-account fetch
/// caches are dropped since they now describe the wrong credentials.
#[test]
fn overwrite_captured_profile_keeps_config_and_history_swaps_credentials() {
    let _home = HomeSandbox::new();

    // "acme" sits in the MIDDLE of a 3-profile chain — a blind delete+append
    // would move it to the end, so this actually proves position survives an
    // in-place mutation rather than merely proving membership.
    let first = Profile::new("first".to_string(), None, None);
    save_profile(&first).expect("save first");
    let last = Profile::new("last".to_string(), None, None);
    save_profile(&last).expect("save last");

    let mut target = Profile::new("acme".to_string(), None, None);
    target.auto_start = true;
    target.env.insert("FOO".to_string(), "bar".to_string());
    target.fallback_threshold = Some(42.0);
    target.bell_threshold = Some(77.0);
    target.models.opus = Some("claude-opus-4".to_string());
    target.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&target).expect("save target");

    let history_path = profile_dir("acme").unwrap().join("usage_history.jsonl");
    std::fs::write(&history_path, b"{\"ts\":1}\n").expect("seed usage history");

    // Seed the transient fetch-state caches the overwrite must drop.
    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        crate::profile_cache::write_profile_cache("acme", file, &"stale");
    }

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["first".into(), "acme".into(), "last".into()],
            fallback_chain: vec!["first".into(), "acme".into(), "last".into()],
            active_profile: Some("first".into()),
            ..AppState::default()
        },
        profiles: vec![first, target, last],
    };

    let snapshot = CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "new-access".to_string(),
                refresh_token: Some("new-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("new-api-key".to_string()),
        identity: crate::actions::CaptureIdentity::LiveLogin,
    };

    overwrite_captured_profile(&mut config, "acme", snapshot).expect("overwrite in place");

    assert_eq!(
        config.profiles.len(),
        3,
        "no duplicate entry from a blind append"
    );
    let acme = config
        .find("acme")
        .expect("profile still present under the same name");
    assert_eq!(
        acme.access_token(),
        Some("new-access"),
        "credentials replaced"
    );
    assert_eq!(
        acme.base_url.as_deref(),
        Some("https://api.example.com"),
        "base_url replaced"
    );
    assert_eq!(
        acme.api_key.as_deref(),
        Some("new-api-key"),
        "api_key replaced"
    );
    assert!(acme.auto_start, "auto_start config preserved");
    assert_eq!(
        acme.env.get("FOO"),
        Some(&"bar".to_string()),
        "env map preserved"
    );
    assert_eq!(
        acme.fallback_threshold,
        Some(42.0),
        "fallback_threshold preserved"
    );
    assert_eq!(acme.bell_threshold, Some(77.0), "bell_threshold preserved");
    assert_eq!(
        acme.models.opus.as_deref(),
        Some("claude-opus-4"),
        "model settings preserved"
    );
    assert!(
        acme.usage.is_none() && acme.fetch_status.is_none() && acme.third_party_usage.is_none(),
        "transient fetch state cleared"
    );

    assert_eq!(
        config.state.fallback_chain,
        vec![
            crate::profile::ProfileName::from("first"),
            crate::profile::ProfileName::from("acme"),
            crate::profile::ProfileName::from("last"),
        ],
        "chain position must survive an in-place overwrite, not delete+append"
    );

    assert_eq!(
        std::fs::read_to_string(&history_path).unwrap(),
        "{\"ts\":1}\n",
        "usage_history.jsonl is the persisted log, not a cache — must survive"
    );

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        let path = crate::profile_cache::profile_cache_path("acme", file).unwrap();
        assert!(
            !path.exists(),
            "{file} must be dropped — it describes the old account"
        );
    }
}

/// Reachable via login → switch away → disable → delete the (now-inactive)
/// active (clears `active_profile` to `None` — `AppConfig::remove`) →
/// `clauth login <disabled>`, the documented revoked-token recovery: the
/// auto-activate branch must never make a disabled profile active, though it
/// still captures the fresh credentials the operator asked for. Condensed to
/// the minimal repro: a disabled profile + no active profile + a reauth
/// capture.
#[test]
fn overwrite_captured_profile_does_not_auto_activate_a_disabled_profile() {
    let _home = HomeSandbox::new();
    let mut target = Profile::new("acme".to_string(), None, None);
    target.disabled = true;
    save_profile(&target).expect("save target");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            active_profile: None,
            ..AppState::default()
        },
        profiles: vec![target],
    };

    overwrite_captured_profile(&mut config, "acme", login_snapshot("fresh-refresh", None))
        .expect("capture succeeds");

    assert_eq!(
        config.state.active_profile, None,
        "a disabled profile must never be auto-activated, even with no active profile at all"
    );
    assert_eq!(
        config.find("acme").unwrap().access_token(),
        Some("acc"),
        "the fresh credentials must still be captured"
    );
}

// ── /profile TTL clock across account swaps ─────────────────────────────────
//
// The clock is per profile NAME but describes the account behind it. Swapping a
// different account onto a name (reauth, capture-overwrite, adopt-divergence)
// leaves an anchored profile whose stamp belongs to the old account — the anchor
// gate can't catch that, so the new account's tier would go unfetched for up to
// an hour, with `usage_cache.json` just dropped and nothing to render meanwhile.
// Asserting through `take_profile_fetch` (not the stamp file) covers BOTH halves:
// the TUI swaps in-process, where a surviving memo outranks a deleted file.

/// Save `name` as an ANCHORED profile — the anchor is what makes the durable half
/// of its clock count — and arm the clock exactly as a live fetch would.
fn armed_ttl_profile(name: &str, t0: u64) -> Profile {
    let profile = Profile::new(name.to_string(), None, None);
    save_profile(&profile).expect("save profile");
    crate::profile_cache::write_profile_cache(
        name,
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-old-account".to_string(),
    );
    assert!(
        crate::usage::take_profile_fetch(name, false, t0),
        "the first attempt arms the clock"
    );
    assert!(
        !crate::usage::take_profile_fetch(name, false, t0 + 60_000),
        "precondition: the clock is armed and would mute /profile"
    );
    profile
}

/// Config holding `profile`, with a different profile marked active so the swap
/// paths skip their live-relink branches.
fn inactive_config(profile: Profile) -> AppConfig {
    AppConfig {
        state: AppState {
            profiles: vec![profile.name.clone()],
            fallback_chain: vec![profile.name.clone()],
            active_profile: Some("someone-else".into()),
            ..AppState::default()
        },
        profiles: vec![profile],
    }
}

// ── identity anchor rides the snapshot into the commit ───────────────────────
//
// The uuid an interactive login probed is only trustworthy once the credentials
// it belongs to are actually stored. Every path (CLI reauth, CLI new, TUI silent,
// TUI confirm-gated, session-save) funnels through these two fns, so they own the
// seeding — no call site does, and none can forget to.

/// Read the on-disk identity anchor for `name`.
fn anchor_of(name: &str) -> Option<String> {
    crate::profile_cache::load_profile_cache::<String>(
        name,
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
    )
}

/// An OAuth snapshot as a completed login hands it over.
fn login_snapshot(refresh: &str, account_uuid: Option<&str>) -> CaptureSnapshot {
    CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "acc".to_string(),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        base_url: None,
        api_key: None,
        identity: account_uuid.map_or(crate::actions::CaptureIdentity::Unknown, |u| {
            crate::actions::CaptureIdentity::Known(crate::usage::AccountIdentity {
                uuid: u.to_string(),
                email: None,
            })
        }),
    }
}

#[test]
fn overwrite_captured_profile_anchors_the_account_it_committed() {
    let _home = HomeSandbox::new();
    let target = Profile::new("swap".to_string(), None, None);
    save_profile(&target).expect("save target");
    let mut config = inactive_config(target);
    crate::usage::seed_login_anchor("swap", Some("uuid-old-account"));

    overwrite_captured_profile(&mut config, "swap", login_snapshot("new", Some("uuid-new")))
        .expect("overwrite in place");

    assert_eq!(
        anchor_of("swap").as_deref(),
        Some("uuid-new"),
        "a reauth onto a DIFFERENT account replaces the anchor it invalidated"
    );
}

#[test]
fn capture_into_profile_anchors_the_account_it_committed() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };

    capture_into_profile(
        &mut config,
        "fresh".to_string(),
        login_snapshot("minted", Some("uuid-fresh")),
    )
    .expect("capture");

    assert_eq!(
        anchor_of("fresh").as_deref(),
        Some("uuid-fresh"),
        "a new account is anchored by the login that created it"
    );
}

#[test]
fn a_snapshot_with_no_proven_identity_drops_the_stale_anchor() {
    let _home = HomeSandbox::new();
    let target = Profile::new("unproven".to_string(), None, None);
    save_profile(&target).expect("save target");
    let mut config = inactive_config(target);
    crate::usage::seed_login_anchor("unproven", Some("uuid-existing"));

    // CAP-1/CAP-2: the anchor must move with the store. An overwrite whose
    // identity is unproven may have swapped a DIFFERENT account onto the name;
    // keeping the old anchor would let it vouch for credentials it may not
    // match (the 2026-07-12 mis-routing). Refuse-and-heal: drop it and let the
    // hourly `/profile` backfill re-seed the truth.
    overwrite_captured_profile(&mut config, "unproven", login_snapshot("new", None))
        .expect("overwrite in place");

    assert_eq!(
        anchor_of("unproven"),
        None,
        "an unproven swap drops the stale anchor rather than letting it vouch \
         for credentials it may not match"
    );
}

#[test]
fn overwrite_captured_profile_expires_the_profile_ttl_clock() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-swap", t0));

    overwrite_captured_profile(
        &mut config,
        "ttl-swap",
        CaptureSnapshot {
            credentials: None,
            base_url: Some("https://api.example.com".to_string()),
            api_key: Some("new-api-key".to_string()),
            identity: crate::actions::CaptureIdentity::Unknown,
        },
    )
    .expect("overwrite in place");

    assert!(
        crate::usage::take_profile_fetch("ttl-swap", false, t0 + 120_000),
        "the swapped-in account must pull its own /profile now, not up to an hour later"
    );
}

#[test]
fn clear_profile_credentials_expires_the_profile_ttl_clock() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-logout", t0));

    clear_profile_credentials(&mut config, "ttl-logout").expect("log out");

    assert!(
        crate::usage::take_profile_fetch("ttl-logout", false, t0 + 120_000),
        "a re-login into the blanked shell must pull its own tier, not the old account's clock"
    );
}

#[test]
fn delete_profile_expires_the_profile_ttl_clock() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-del", t0));

    delete_profile(&mut config, "ttl-del", false).expect("delete");

    // `remove_dir_all` took the durable stamp; only the memo could survive here,
    // and it would mute the first /profile of a same-name relogin in this process.
    assert!(
        crate::usage::take_profile_fetch("ttl-del", false, t0 + 120_000),
        "the memo must not outlive the profile it describes"
    );
}

#[test]
fn rename_profile_expires_the_old_names_ttl_clock_and_carries_the_stamp() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-ren-old", t0));

    rename_profile(&mut config, "ttl-ren-old", "ttl-ren-new").expect("rename");

    assert!(
        crate::usage::take_profile_fetch("ttl-ren-old", false, t0 + 120_000),
        "the old name's memo is stranded over a stamp that moved away — expire it"
    );
    // Same account, same clock: the dir move carried the anchor and the stamp, so
    // the new name inherits the hour rather than paying a fresh /profile for it.
    assert!(
        !crate::usage::take_profile_fetch("ttl-ren-new", false, t0 + 120_000),
        "a rename is not an account swap — the new name reuses the durable stamp"
    );
}

/// A reauth overwrite replaces the dead credential chain — the whole point of
/// re-logging in — so it must lift the profile's `auth_broken` quarantine,
/// exactly like the fresh-capture path (`capture_into_profile`) does. Left
/// set, the flag keeps the just-relogged account excluded from every chain
/// walk and keeps the "login expired" banner up (observed 2026-07-09: a
/// re-login via the menu bar left the profile quarantined).
#[test]
fn overwrite_captured_profile_clears_auth_broken_quarantine() {
    let _home = HomeSandbox::new();

    let first = Profile::new("first".to_string(), None, None);
    save_profile(&first).expect("save first");
    let target = Profile::new("acme".to_string(), None, None);
    save_profile(&target).expect("save target");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["first".into(), "acme".into()],
            fallback_chain: vec!["first".into(), "acme".into()],
            active_profile: Some("first".into()),
            auth_broken: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![first, target],
    };

    let snapshot = CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        base_url: None,
        api_key: None,
        identity: crate::actions::CaptureIdentity::LiveLogin,
    };
    overwrite_captured_profile(&mut config, "acme", snapshot).expect("overwrite");

    assert!(
        !config.is_auth_broken("acme"),
        "in-memory quarantine must lift with the fresh credentials"
    );
    let persisted = crate::profile::load_config().expect("reload").state;
    assert!(
        !persisted.auth_broken.iter().any(|n| n.as_str() == "acme"),
        "persisted quarantine must lift too"
    );
}

/// Overwriting the ACTIVE profile must re-apply to live `~/.claude` state —
/// mirrors `edit_profile_endpoint`'s active-case handling. Without this a
/// running `claude` keeps reading the OLD endpoint/token until the next
/// explicit switch, and dropping OAuth creds on an active profile (a
/// third-party recapture) would leave `.credentials.json` a dangling
/// symlink instead of a clean absence.
#[test]
fn overwrite_captured_profile_reapplies_live_state_when_active() {
    let _home = HomeSandbox::new();

    let mut acme = Profile::new("acme".to_string(), None, None);
    acme.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&acme).expect("save acme");
    crate::claude::link_profile_credentials("acme").expect("link acme live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            fallback_chain: vec!["acme".into()],
            active_profile: Some("acme".into()),
            ..AppState::default()
        },
        profiles: vec![acme],
    };

    // Overwrite the active profile with a third-party (no-OAuth) snapshot.
    let snapshot = CaptureSnapshot {
        credentials: None,
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("new-api-key".to_string()),
        identity: crate::actions::CaptureIdentity::LiveLogin,
    };
    overwrite_captured_profile(&mut config, "acme", snapshot).expect("overwrite active profile");

    let live_endpoint = crate::claude::read_claude_endpoint_config().expect("read live endpoint");
    assert_eq!(
        live_endpoint.base_url.as_deref(),
        Some("https://api.example.com"),
        "live settings.json must pick up the new base_url immediately, not on next switch"
    );
    assert_eq!(
        live_endpoint.api_key.as_deref(),
        Some("new-api-key"),
        "live settings.json must pick up the new api_key immediately, not on next switch"
    );

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    assert!(
        live_path.symlink_metadata().is_err(),
        "no dangling .credentials.json symlink after credentials go to None while active"
    );
}

/// Deleting the ACTIVE API-key profile must strip its endpoint + key from the
/// live `~/.claude/settings.json`, not only the (absent) credentials link.
/// Otherwise the deleted account's `ANTHROPIC_AUTH_TOKEN` lingers in plaintext
/// and the next session still routes to the dead endpoint.
#[test]
fn delete_active_api_profile_unwires_settings_endpoint() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(
        &mut config,
        "api-acct".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
        None,
    )
    .expect("create api profile");
    // create_blank_profile does not activate; mark it active and wire the live
    // settings.json the way a switch would, then delete it out from under that.
    config.state.active_profile = Some("api-acct".into());
    let profile = config.find("api-acct").expect("profile present").clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");
    assert_eq!(
        crate::claude::read_claude_endpoint_config()
            .expect("read endpoint")
            .api_key
            .as_deref(),
        Some("sk-secret"),
        "precondition: active api key is wired into settings.json"
    );

    delete_profile(&mut config, "api-acct", false).expect("delete active api profile");

    let after = crate::claude::read_claude_endpoint_config().expect("read endpoint");
    assert_eq!(
        after.base_url, None,
        "deleted endpoint must not linger in settings.json"
    );
    assert_eq!(
        after.api_key, None,
        "deleted api key must not linger in settings.json"
    );
}

/// #4: a profile held by a live `clauth start` session must not be deleted
/// without `--force` — the running session's account can't be pulled out from
/// under it. An unforced delete refuses and leaves the record intact; `force`
/// overrides and removes it.
#[test]
fn delete_refuses_live_session_unless_forced() {
    let home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "busy".to_string(), None, None, None)
        .expect("create profile");

    // Simulate a live session: a locked pid file in the profile's sessions dir
    // reads as alive via `has_live_session` (the probe's `try_lock` on a
    // separate fd fails while this fd holds the flock).
    let sessions = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("busy")
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("open pid");
    pid.lock().expect("lock pid");

    let refused = delete_profile(&mut config, "busy", false);
    assert!(
        refused.is_err(),
        "a live session must block an unforced delete"
    );
    assert!(
        config.find("busy").is_some(),
        "the refused delete must leave the profile record intact"
    );

    delete_profile(&mut config, "busy", true).expect("force overrides the live-session guard");
    assert!(
        config.find("busy").is_none(),
        "force must remove the profile despite the live session"
    );
}

// ── disable_profile / enable_profile ────────────────────────────────────────

#[test]
fn disable_refuses_the_active_profile() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "acme".to_string(), None, None, None).expect("create");
    config.state.active_profile = Some("acme".into());

    let err = disable_profile(&mut config, "acme").expect_err("active profile must be refused");
    assert_eq!(
        err.to_string(),
        "'acme' is the active account — switch away first"
    );
    assert!(
        !config.find("acme").unwrap().is_disabled(),
        "a refused disable must leave the flag untouched"
    );
}

#[test]
fn disable_refuses_a_profile_with_a_live_session() {
    let home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "busy".to_string(), None, None, None).expect("create");

    // Same live-session simulation as `delete_refuses_live_session_unless_forced`:
    // a locked pid file in the profile's sessions dir reads as alive.
    let sessions = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("busy")
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("open pid");
    pid.lock().expect("lock pid");

    let err = disable_profile(&mut config, "busy").expect_err("a live session must be refused");
    assert_eq!(
        err.to_string(),
        "'busy' has an open session — close it first"
    );
    assert!(
        !config.find("busy").unwrap().is_disabled(),
        "a refused disable must leave the flag untouched"
    );
}

#[test]
fn disable_sets_the_flag_and_a_reload_observes_it() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "acme".to_string(), None, None, None).expect("create");

    let changed = disable_profile(&mut config, "acme").expect("disable succeeds");
    assert!(changed, "a fresh disable must report a real change");
    assert!(config.find("acme").unwrap().is_disabled());

    let reloaded = crate::profile::load_config().expect("reload from disk");
    assert!(
        reloaded.find("acme").unwrap().is_disabled(),
        "the flag must survive a reload from disk"
    );
}

#[test]
fn disable_is_idempotent_on_an_already_disabled_profile() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "acme".to_string(), None, None, None).expect("create");
    disable_profile(&mut config, "acme").expect("first disable");

    let changed = disable_profile(&mut config, "acme").expect("re-disable is not an error");
    assert!(!changed, "a no-op disable must report no change");
    assert!(config.find("acme").unwrap().is_disabled());
}

#[test]
fn enable_clears_the_flag_leaving_everything_else_byte_identical() {
    let _home = HomeSandbox::new();
    let mut profile = Profile::new("acme".to_string(), None, None);
    profile.env.insert("FOO".to_string(), "bar".to_string());
    profile.fallback_threshold = Some(42.0);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-acme".to_string(),
            refresh_token: Some("rt-acme".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![profile],
    };

    let config_path = crate::profile::profile_subpath("acme", "config.toml").unwrap();
    let creds_path = crate::profile::profile_subpath("acme", "credentials.json").unwrap();
    let config_before = std::fs::read(&config_path).unwrap();
    let creds_before = std::fs::read(&creds_path).unwrap();

    disable_profile(&mut config, "acme").expect("disable");
    assert!(config.find("acme").unwrap().is_disabled());

    let changed = enable_profile(&mut config, "acme").expect("enable succeeds");
    assert!(changed, "a real re-enable must report a change");

    let acme = config.find("acme").unwrap();
    assert!(!acme.is_disabled());
    assert_eq!(
        acme.env.get("FOO"),
        Some(&"bar".to_string()),
        "env untouched"
    );
    assert_eq!(
        acme.fallback_threshold,
        Some(42.0),
        "fallback_threshold untouched"
    );
    assert_eq!(
        acme.access_token(),
        Some("at-acme"),
        "credentials untouched"
    );

    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        config_before,
        "config.toml must round-trip byte-identical once re-enabled"
    );
    assert_eq!(
        std::fs::read(&creds_path).unwrap(),
        creds_before,
        "credentials.json must round-trip byte-identical once re-enabled"
    );
}

#[test]
fn enable_is_idempotent_on_an_already_enabled_profile() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "acme".to_string(), None, None, None).expect("create");

    let changed = enable_profile(&mut config, "acme").expect("enable on an enabled profile");
    assert!(!changed, "a no-op enable must report no change");
    assert!(!config.find("acme").unwrap().is_disabled());
}

/// #5: for an ACTIVE profile the settings-unwire (a fallible external write) must
/// run BEFORE any irreversible local removal. When the unwire fails, the whole
/// delete fails leaving BOTH the record and the dir intact and fully retryable —
/// not the account stranded in live settings.json with its record already gone.
#[test]
fn delete_active_unwire_failure_keeps_profile_retryable() {
    let home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(
        &mut config,
        "api-acct".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
        None,
    )
    .expect("create api profile");
    config.state.active_profile = Some("api-acct".into());

    // Force the settings unwire to fail: make ~/.claude/settings.json a directory
    // so the merge-read inside `apply_profile_to_claude_settings` errors before
    // any write. Deterministic and root-safe (a read-only chmod would be bypassed
    // when the suite runs as root).
    let settings = home.home().join(".claude").join("settings.json");
    std::fs::create_dir_all(&settings).expect("settings.json as dir");

    let result = delete_profile(&mut config, "api-acct", false);
    assert!(
        result.is_err(),
        "a failed settings unwire must fail the whole delete"
    );
    assert!(
        config.find("api-acct").is_some(),
        "a failed unwire must leave the profile record intact and retryable"
    );
}

/// Setup-tab "log out" on an ACTIVE API account drops only the api key: the base
/// url stays wired (account keeps its API shell + active status), the live
/// `settings.json` loses `ANTHROPIC_AUTH_TOKEN` but keeps `ANTHROPIC_BASE_URL`,
/// and the stale third-party stats cache is removed.
#[test]
fn clear_profile_api_key_keeps_base_url_and_active_status() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(
        &mut config,
        "api-acct".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
        None,
    )
    .expect("create api profile");
    config.state.active_profile = Some("api-acct".into());
    let profile = config.find("api-acct").expect("profile present").clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");
    crate::profile_cache::write_profile_cache(
        "api-acct",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &"stale",
    );

    clear_profile_api_key(&mut config, "api-acct").expect("clear api key");

    let profile = config.find("api-acct").expect("profile still present");
    assert_eq!(profile.api_key, None, "api key dropped");
    assert_eq!(
        profile.base_url.as_deref(),
        Some("https://api.example.com"),
        "base-url shell preserved"
    );
    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("api-acct"),
        "account stays active (only the key is gone)"
    );

    let after = crate::claude::read_claude_endpoint_config().expect("read endpoint");
    assert_eq!(
        after.base_url.as_deref(),
        Some("https://api.example.com"),
        "live base url kept so the account stays an API shell"
    );
    assert_eq!(after.api_key, None, "live auth token stripped on log out");

    let cache = crate::profile_cache::profile_cache_path(
        "api-acct",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    assert!(!cache.exists(), "stale third-party stats cache dropped");

    // The leak fix drops a base_url at the LOAD boundary, so verify the shell
    // survives a reload too: a cleared PURE api account (no OAuth pair) keeps its
    // base_url and is not flipped to an OAuth profile.
    let reloaded = crate::profile::load_profile("api-acct").expect("reload");
    assert_eq!(
        reloaded.base_url.as_deref(),
        Some("https://api.example.com"),
        "a cleared api account keeps its base_url shell across a reload"
    );
    assert_eq!(reloaded.api_key, None, "still no key after reload");
}

/// Blanking an active profile drops its credentials + per-account fetch caches
/// and clears the live link + `active_profile`, while name/env/model survive.
#[test]
fn clear_profile_credentials_blanks_active_profile_keeping_shell() {
    let _home = HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.auto_start = true;
    acct.env.insert("FOO".to_string(), "bar".to_string());
    acct.models.opus = Some("claude-opus-4".to_string());
    acct.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&acct).expect("save acct");
    crate::claude::link_profile_credentials("acct").expect("link acct live");

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        crate::profile_cache::write_profile_cache("acct", file, &"stale");
    }

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into()],
            fallback_chain: vec!["acct".into()],
            active_profile: Some("acct".into()),
            ..AppState::default()
        },
        profiles: vec![acct],
    };

    clear_profile_credentials(&mut config, "acct").expect("clear credentials");

    let profile = config.find("acct").expect("profile still present");
    assert!(profile.credentials.is_none(), "credentials dropped");
    assert!(profile.auto_start, "shell preserved: auto_start");
    assert_eq!(
        profile.env.get("FOO"),
        Some(&"bar".to_string()),
        "shell preserved: env"
    );
    assert_eq!(
        profile.models.opus.as_deref(),
        Some("claude-opus-4"),
        "shell preserved: model"
    );
    assert!(
        config.state.active_profile.is_none(),
        "active profile deactivated"
    );

    let cred_path = profile_dir("acct").unwrap().join("credentials.json");
    assert!(!cred_path.exists(), "credentials.json removed");

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        let path = crate::profile_cache::profile_cache_path("acct", file).unwrap();
        assert!(!path.exists(), "{file} must be dropped");
    }

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    assert!(
        live_path.symlink_metadata().is_err(),
        "live .credentials.json link cleared on blanking the active profile"
    );
}

/// Blanking a NON-active profile must not touch the active link / `active_profile`,
/// and a lingering rotation sidecar must not resurrect the deleted login on the
/// next disk load (`recover_pending_credentials` treats a missing credentials.json
/// as a failed commit and adopts the sidecar).
#[test]
fn clear_profile_credentials_non_active_and_no_sidecar_resurrection() {
    let _home = HomeSandbox::new();

    let creds = || ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(creds());
    save_profile(&acct).expect("save acct");
    // A rotation sidecar that never committed — the resurrection vector.
    crate::profile::stage_rotated_credentials("acct", &creds()).expect("stage sidecar");

    let mut other = Profile::new("other".to_string(), None, None);
    other.credentials = Some(creds());
    save_profile(&other).expect("save other");
    crate::claude::link_profile_credentials("other").expect("link other live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into(), "other".into()],
            fallback_chain: vec!["acct".into(), "other".into()],
            active_profile: Some("other".into()),
            ..AppState::default()
        },
        profiles: vec![acct, other],
    };

    // Persist the profile list so `load_config` below can find both by name.
    crate::profile::save_app_state(&config.state).expect("persist state");

    clear_profile_credentials(&mut config, "acct").expect("clear credentials");

    // The active profile and its live link are untouched — only "acct" changed.
    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("other"),
        "blanking a non-active profile leaves the active one set"
    );
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    assert!(
        live_path.symlink_metadata().is_ok(),
        "the active profile's live link survives a non-active blank"
    );

    // Reload from disk: the sidecar must be gone, so the login stays deleted.
    let reloaded = crate::profile::load_config().expect("reload config");
    let acct = reloaded.find("acct").expect("acct still present");
    assert!(
        acct.credentials.is_none(),
        "a lingering sidecar must not resurrect the blanked login"
    );
    let cred_path = profile_dir("acct").unwrap().join("credentials.json");
    assert!(
        !cred_path.exists(),
        "credentials.json stays gone after reload (sidecar not adopted)"
    );
}

// ── issue #17: stale oauthAccount deleted on every switch path ────────────

fn home_claude_json_path() -> std::path::PathBuf {
    crate::profile::home_dir().unwrap().join(".claude.json")
}

fn write_home_claude_json_with_identity() {
    std::fs::write(
        home_claude_json_path(),
        serde_json::to_vec_pretty(&serde_json::json!({
            "oauthAccount": {"emailAddress": "stale@x"},
            "numStartups": 7,
        }))
        .unwrap(),
    )
    .expect("write home .claude.json");
}

/// `finish_switch` is the shared convergence point for the manual CLI, TUI,
/// MCP `switch`, and fallback switch paths (all four route through
/// `switch_profile`/`switch_profile_reconciled`/`switch_profile_discard`,
/// which call it under the state lock) — asserting on it directly pins the
/// behaviour for all of them at once.
#[test]
fn finish_switch_deletes_stale_oauth_account_block() {
    let _home = HomeSandbox::new();
    write_home_claude_json_with_identity();

    let mut config = acct_config();
    finish_switch(&mut config, "acct").expect("finish_switch");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home_claude_json_path()).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "the outgoing account's identity block must be gone after a switch"
    );
    assert_eq!(
        after["numStartups"],
        serde_json::json!(7),
        "unrelated keys must survive the switch untouched"
    );
}

/// `switch_off` (chain-exhausted / manual "turn off") clears live credentials
/// without going through `finish_switch` — a stale identity block is just as
/// wrong once creds are gone, so it needs its own coverage rather than relying
/// on the shared path.
#[test]
fn switch_off_also_deletes_stale_oauth_account_block() {
    let _home = HomeSandbox::new();
    write_home_claude_json_with_identity();

    let profile = Profile::new("acct".to_string(), None, None);
    save_profile(&profile).expect("save profile");
    crate::claude::link_profile_credentials("acct").expect("link acct live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into()],
            active_profile: Some("acct".into()),
            ..AppState::default()
        },
        profiles: vec![profile],
    };

    switch_off(&mut config).expect("switch_off");

    assert!(config.state.active_profile.is_none());
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home_claude_json_path()).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "no active account remains, so the stale identity block must be gone too"
    );
    assert_eq!(after["numStartups"], serde_json::json!(7));
}

/// `switch_off` on a DIVERGED live file: the foreign `/login` is dropped, never
/// absorbed. `snapshot_active_credentials` skips a diverged file so the profile
/// keeps its stored identity while the live creds are cleared; the divergence
/// flow's consent prompt is what stands between the user and this drop.
#[test]
fn switch_off_on_diverged_file_keeps_profile_snapshot_and_drops_login() {
    let _home = HomeSandbox::new();

    let mut profile = Profile::new("acct".to_string(), None, None);
    profile.credentials = Some(oauth_creds("stored-token"));
    save_profile(&profile).expect("save profile");

    // A plain file with a different token where the symlink should sit = Diverged.
    let live = _home.home().join(".claude").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("fresh-login")).expect("serialize"),
    )
    .expect("write diverged live file");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into()],
            active_profile: Some("acct".into()),
            ..AppState::default()
        },
        profiles: vec![profile],
    };

    switch_off(&mut config).expect("switch_off");

    assert!(config.state.active_profile.is_none());
    assert!(
        !live.exists(),
        "live creds cleared: the fresh login is gone"
    );
    assert_eq!(
        config.profiles[0]
            .credentials
            .as_ref()
            .and_then(|c| c.access_token()),
        Some("stored-token"),
        "a foreign login must never be absorbed into the profile snapshot"
    );
}

fn oauth_creds(access: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// AUTH-1 reauth: `clauth login <existing>` overwrites a quarantined profile's
/// stored tokens through `overwrite_captured_profile` — the documented recovery
/// for a revoked login — and must clear its auth-broken flag so the recovered
/// account rejoins the fallback chain and is a valid switch target again. The
/// active-but-dead account here is the Incident C scenario.
#[test]
fn reauth_overwrite_clears_broken_flag() {
    let _home = HomeSandbox::new();

    let mut stale = Profile::new("xfx".to_string(), None, None);
    stale.credentials = Some(oauth_creds("stale-access"));
    save_profile(&stale).expect("save profile");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["xfx".into()],
            active_profile: Some("xfx".into()),
            ..AppState::default()
        },
        profiles: vec![stale],
    };
    config.set_auth_broken("xfx", true);
    assert!(config.is_auth_broken("xfx"), "precondition: quarantined");

    overwrite_captured_profile(
        &mut config,
        "xfx",
        CaptureSnapshot {
            credentials: Some(oauth_creds("fresh-access")),
            base_url: None,
            api_key: None,
            identity: crate::actions::CaptureIdentity::Unknown,
        },
    )
    .expect("re-auth overwrite");

    assert_eq!(
        config.find("xfx").and_then(|p| p.access_token()),
        Some("fresh-access"),
        "credentials overwritten by re-auth",
    );
    assert!(
        !config.is_auth_broken("xfx"),
        "auth-broken quarantine cleared by re-auth",
    );
}

/// AUTH-1 switch gate (Incident C): a CLI switch to a target whose OAuth login
/// is dead — expired access token, no refresh token, so unrecoverable without a
/// re-login — is refused with the exact `clauth login <name>` recovery hint
/// instead of installing the dead token into the Keychain. The no-refresh-token
/// path reaches `AuthGate::Broken` with no network call, so the assertion stays
/// hermetic.
#[test]
fn switch_cli_refuses_dead_target_with_login_hint() {
    let _home = HomeSandbox::new();

    let mut dead = Profile::new("dead-acct".to_string(), None, None);
    dead.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "expired".to_string(),
            refresh_token: None,
            expires_at: Some(1), // epoch-ms 1 → long expired
            scopes: None,
            subscription_type: None,
        }),
    });

    let config = AppConfig {
        state: AppState {
            profiles: vec!["dead-acct".into()],
            active_profile: None, // no outgoing profile → no link reconcile before the gate
            ..AppState::default()
        },
        profiles: vec![dead],
    };

    let err = switch_profile_cli(config, "dead-acct").expect_err("a dead target must be refused");
    assert!(
        err.to_string().contains("clauth login dead-acct"),
        "the refusal must name the recovery command, got: {err}",
    );
}

// ── identify_live_login_owner: whose login sits in ~/.claude right now ──────
//
// Two tiers: token equality (authoritative), then account uuid (fallback for a
// sibling's CC re-login that mints fresh tokens no stored pair recognizes).

#[cfg(unix)]
mod identify_live_login_owner {
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    use crate::testutil::HomeSandbox;

    fn creds(access: &str, refresh: &str) -> ClaudeCredentials {
        ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn config_with(profiles: Vec<(&str, ClaudeCredentials)>) -> AppConfig {
        let profiles: Vec<Profile> = profiles
            .into_iter()
            .map(|(name, c)| {
                let mut p = Profile::new(name.to_string(), None, None);
                p.credentials = Some(c);
                p
            })
            .collect();
        AppConfig {
            state: AppState {
                profiles: profiles.iter().map(|p| p.name.clone()).collect(),
                ..AppState::default()
            },
            profiles,
        }
    }

    fn write_live(c: &ClaudeCredentials) {
        let live = crate::profile::claude_dir()
            .expect("claude dir")
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
        std::fs::write(&live, serde_json::to_vec(c).expect("ser")).expect("write");
    }

    /// Tier 1: exact token equality — the live file IS a profile's stored
    /// credential (stale mirror / half-landed switch).
    fn home_claude_json() -> std::path::PathBuf {
        crate::profile::home_dir().unwrap().join(".claude.json")
    }

    /// Write `~/.claude.json` carrying an `oauthAccount.accountUuid` of `uuid`,
    /// or a file with no `oauthAccount` block at all when `uuid` is `None`.
    fn write_home_identity(uuid: Option<&str>) {
        let mut obj = serde_json::json!({"numStartups": 1});
        if let Some(u) = uuid {
            obj["oauthAccount"] = serde_json::json!({"accountUuid": u});
        }
        std::fs::write(home_claude_json(), serde_json::to_vec_pretty(&obj).unwrap()).unwrap();
    }

    fn anchor(name: &str, uuid: &str) {
        crate::profile_cache::write_profile_cache(
            name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &uuid.to_string(),
        );
    }

    /// Exact token equality — the live file IS a profile's stored credential
    /// (stale mirror / half-landed switch).
    #[test]
    fn exact_token_match_identifies_the_owner() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![
            ("a", creds("at-a", "rt-a")),
            ("b", creds("at-b", "rt-b")),
        ]);
        write_live(&creds("at-b", "rt-b"));
        assert_eq!(
            crate::actions::identify_live_login_owner(&cfg).as_deref(),
            Some("b")
        );
    }

    /// Tier 2: Claude Code's own `~/.claude.json` identity record against a
    /// profile's cached anchor — a CC re-login where every token is new.
    #[test]
    fn claude_json_uuid_matches_a_profile_anchor() {
        let home = HomeSandbox::new();
        let cfg = config_with(vec![
            ("a", creds("at-a", "rt-a")),
            ("b", creds("at-b", "rt-b")),
        ]);
        write_live(&creds("at-brand-new", "rt-brand-new"));
        crate::profile_cache::write_profile_cache(
            "b",
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &"uuid-b".to_string(),
        );
        std::fs::write(
            home.home().join(".claude.json"),
            serde_json::to_vec(&serde_json::json!({
                "oauthAccount": { "accountUuid": "uuid-b" }
            }))
            .expect("ser"),
        )
        .expect("write claude.json");
        assert_eq!(
            crate::actions::identify_live_login_owner(&cfg).as_deref(),
            Some("b")
        );
    }

    /// No token match and no uuid record → unknown; a blank anchor must
    /// never match a blank uuid.
    /// No token match → unknown; a genuinely foreign account (no anchor on the
    /// live identity either) identifies nobody.
    #[test]
    fn a_foreign_login_identifies_nobody() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![("a", creds("at-a", "rt-a"))]);
        write_live(&creds("at-foreign", "rt-foreign"));
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);
    }

    /// Token equality is authoritative: even when CC's identity block points at
    /// a DIFFERENT profile, a matching stored token still wins.
    #[test]
    fn token_tier_wins_over_uuid_tier_when_tokens_match() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![
            ("a", creds("at-a", "rt-a")),
            ("b", creds("at-b", "rt-b")),
        ]);
        // Live = b's exact tokens, but the identity block says a.
        write_live(&creds("at-b", "rt-b"));
        anchor("a", "uuid-a");
        write_home_identity(Some("uuid-a"));
        assert_eq!(
            crate::actions::identify_live_login_owner(&cfg).as_deref(),
            Some("b"),
        );
    }

    /// The fix target: a sibling's CC re-login mints fresh tokens that match no
    /// stored pair, but its `oauthAccount.accountUuid` equals profile b's cached
    /// anchor → returns `Some("b")`. Fails before the uuid tier existed.
    #[test]
    fn uuid_tier_identifies_a_sibling_relogin_when_no_token_matches() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![
            ("a", creds("at-a", "rt-a")),
            ("b", creds("at-b", "rt-b")),
        ]);
        // b re-logged in through CC — every token is new, no stored pair hits.
        write_live(&creds("fresh-at", "fresh-rt"));
        anchor("b", "uuid-b");
        write_home_identity(Some("uuid-b"));
        assert_eq!(
            crate::actions::identify_live_login_owner(&cfg).as_deref(),
            Some("b"),
        );
    }

    #[test]
    fn uuid_tier_no_cached_anchor_identifies_nobody() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![("a", creds("at-a", "rt-a"))]);
        write_live(&creds("fresh-at", "fresh-rt"));
        write_home_identity(Some("uuid-a"));
        // a has NO cached anchor → no match.
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);
    }

    #[test]
    fn uuid_tier_no_oauth_account_block_identifies_nobody() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![("a", creds("at-a", "rt-a"))]);
        write_live(&creds("fresh-at", "fresh-rt"));
        anchor("a", "uuid-a");
        // No oauthAccount block in ~/.claude.json → no live uuid.
        write_home_identity(None);
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);
    }

    /// A blank uuid on either side — and both blank — must never compare equal.
    #[test]
    fn uuid_tier_blank_uuid_on_either_side_identifies_nobody() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![("a", creds("at-a", "rt-a"))]);
        write_live(&creds("fresh-at", "fresh-rt"));

        // Blank live uuid, real anchor.
        write_home_identity(Some("   "));
        anchor("a", "uuid-a");
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);

        // Real live uuid, blank anchor.
        write_home_identity(Some("uuid-a"));
        anchor("a", "   ");
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);

        // Both blank — must never prove identity.
        write_home_identity(Some(""));
        anchor("a", "");
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);
    }

    /// A file caught mid-write by CC (unparseable) yields nobody and is left
    /// byte-for-byte untouched — never clobbered.
    #[test]
    fn uuid_tier_unparseable_claude_json_identifies_nobody_and_leaves_it() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![("a", creds("at-a", "rt-a"))]);
        write_live(&creds("fresh-at", "fresh-rt"));
        anchor("a", "uuid-a");

        let path = home_claude_json();
        let garbage = b"{ this is not valid json ";
        std::fs::write(&path, garbage).unwrap();

        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            garbage,
            "an unparseable file caught mid-write by CC must be left untouched",
        );
    }
}

// ── CAP-1: sanctioned captures move the identity anchor with the store ───────

mod capture_anchor_coherence {
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache, write_profile_cache};
    use crate::testutil::HomeSandbox;

    fn creds(access: &str, refresh: &str) -> ClaudeCredentials {
        ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn config_with_profile(name: &str) -> AppConfig {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds("old-access", "old-refresh"));
        crate::profile::save_profile(&p).expect("save profile");
        AppConfig {
            state: AppState {
                profiles: vec![p.name.clone()],
                active_profile: Some(name.into()),
                ..AppState::default()
            },
            profiles: vec![p],
        }
    }

    fn snapshot(c: ClaudeCredentials) -> crate::actions::CaptureSnapshot {
        crate::actions::CaptureSnapshot {
            credentials: Some(c),
            base_url: None,
            api_key: None,
            identity: crate::actions::CaptureIdentity::LiveLogin,
        }
    }

    /// A sanctioned overwrite may change which ACCOUNT the profile holds — the
    /// anchor must follow the captured login (from CC's `~/.claude.json`
    /// hint), never keep describing the pre-capture account. The stale-anchor
    /// split-brain is how 'ax-backup' polled 'ax-main''s account on 2026-07-12.
    #[test]
    fn overwrite_capture_moves_the_identity_anchor() {
        let _home = HomeSandbox::new();
        let mut cfg = config_with_profile("p");
        write_profile_cache("p", ACCOUNT_ID_CACHE_FILE, &"uuid-old".to_string());
        std::fs::write(
            crate::profile::home_dir()
                .expect("home")
                .join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid-new","emailAddress":"new@example.com"}}"#,
        )
        .expect("write claude.json");

        crate::actions::overwrite_captured_profile(
            &mut cfg,
            "p",
            snapshot(creds("new-access", "new-refresh")),
        )
        .expect("overwrite");

        let anchor: Option<String> = load_profile_cache("p", ACCOUNT_ID_CACHE_FILE);
        assert_eq!(
            anchor.as_deref(),
            Some("uuid-new"),
            "the anchor moves with the captured login",
        );
        let email: Option<String> =
            load_profile_cache("p", crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE);
        assert_eq!(
            email.as_deref(),
            Some("new@example.com"),
            "the email half moves in lockstep with the uuid",
        );
    }

    /// With no local identity hint the stale anchor is DROPPED, not kept: a
    /// missing anchor makes the identity-guarded paths refuse (and the hourly
    /// /profile fetch re-backfills it), while a wrong one re-routes them.
    #[test]
    fn overwrite_capture_drops_a_stale_anchor_without_hint() {
        let _home = HomeSandbox::new();
        let mut cfg = config_with_profile("p");
        write_profile_cache("p", ACCOUNT_ID_CACHE_FILE, &"uuid-old".to_string());

        crate::actions::overwrite_captured_profile(
            &mut cfg,
            "p",
            snapshot(creds("new-access", "new-refresh")),
        )
        .expect("overwrite");

        let anchor: Option<String> = load_profile_cache("p", ACCOUNT_ID_CACHE_FILE);
        assert_eq!(anchor, None, "no hint → the stale anchor is dropped");
    }

    /// CAP-2 regression (2026-07-12 pollution): a browser-minted login KNOWS
    /// its account (probed with the fresh token) — the capture must anchor to
    /// THAT uuid even when CC's live hint names an unrelated account (whatever
    /// login happened to be live during `clauth login <other>`).
    #[test]
    fn browser_mint_anchor_beats_the_live_hint() {
        let _home = HomeSandbox::new();
        let mut cfg = config_with_profile("p");
        write_profile_cache("p", ACCOUNT_ID_CACHE_FILE, &"uuid-old".to_string());
        // A live login for a DIFFERENT account exists — the trap.
        std::fs::write(
            crate::profile::home_dir()
                .expect("home")
                .join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid-unrelated-live"}}"#,
        )
        .expect("write claude.json");

        let mut snap = snapshot(creds("minted-access", "minted-refresh"));
        snap.identity = crate::actions::CaptureIdentity::Known(crate::usage::AccountIdentity {
            uuid: "uuid-probed".to_string(),
            email: Some("probed@example.com".to_string()),
        });
        crate::actions::overwrite_captured_profile(&mut cfg, "p", snap).expect("overwrite");

        let anchor: Option<String> = load_profile_cache("p", ACCOUNT_ID_CACHE_FILE);
        assert_eq!(
            anchor.as_deref(),
            Some("uuid-probed"),
            "the probed identity wins over the unrelated live hint",
        );
        let email: Option<String> =
            load_profile_cache("p", crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE);
        assert_eq!(
            email.as_deref(),
            Some("probed@example.com"),
            "the probed email is anchored alongside the uuid",
        );
    }

    /// CAP-2: an UNPROBED browser mint (TUI re-login flow) must not guess from
    /// the live hint — the stale anchor is dropped and the usage fetcher's
    /// backfill seeds the truth on first fetch.
    #[test]
    fn unprobed_mint_drops_the_anchor_despite_a_live_hint() {
        let _home = HomeSandbox::new();
        let mut cfg = config_with_profile("p");
        write_profile_cache("p", ACCOUNT_ID_CACHE_FILE, &"uuid-old".to_string());
        std::fs::write(
            crate::profile::home_dir()
                .expect("home")
                .join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid-unrelated-live"}}"#,
        )
        .expect("write claude.json");

        let mut snap = snapshot(creds("minted-access", "minted-refresh"));
        snap.identity = crate::actions::CaptureIdentity::Unknown;
        crate::actions::overwrite_captured_profile(&mut cfg, "p", snap).expect("overwrite");

        let anchor: Option<String> = load_profile_cache("p", ACCOUNT_ID_CACHE_FILE);
        assert_eq!(
            anchor, None,
            "no probed identity → drop rather than mis-anchor from the live hint",
        );
    }

    /// CAP-2: `capture_into_profile` (fresh profile) takes the probed identity
    /// the same way — `clauth login <new-name>` anchors to the minted account.
    #[test]
    fn fresh_capture_anchors_to_the_probed_identity() {
        let _home = HomeSandbox::new();
        let mut cfg = AppConfig {
            state: AppState::default(),
            profiles: vec![],
        };
        std::fs::write(
            crate::profile::home_dir()
                .expect("home")
                .join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"uuid-unrelated-live"}}"#,
        )
        .expect("write claude.json");

        let mut snap = snapshot(creds("minted-access", "minted-refresh"));
        snap.identity = crate::actions::CaptureIdentity::Known(crate::usage::AccountIdentity {
            uuid: "uuid-probed".to_string(),
            email: Some("probed@example.com".to_string()),
        });
        crate::actions::capture_into_profile(&mut cfg, "fresh".to_string(), snap).expect("capture");

        let anchor: Option<String> = load_profile_cache("fresh", ACCOUNT_ID_CACHE_FILE);
        assert_eq!(anchor.as_deref(), Some("uuid-probed"));
    }

    /// CAP-3: `account_owner` names the sibling already holding an account —
    /// by uuid (authoritative) or by cached email (case-insensitive fallback
    /// for a profile whose uuid anchor hasn't backfilled) — and never names
    /// the profile being (re-)logged-in itself.
    #[test]
    fn account_owner_names_the_sibling_holding_the_account() {
        use crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE;
        let _home = HomeSandbox::new();
        let mut cfg = config_with_profile("a");
        let mut b = Profile::new("b".to_string(), None, None);
        b.credentials = Some(creds("b-access", "b-refresh"));
        crate::profile::save_profile(&b).expect("save b");
        cfg.profiles.push(b);
        write_profile_cache("a", ACCOUNT_ID_CACHE_FILE, &"uuid-1".to_string());
        write_profile_cache("a", ACCOUNT_EMAIL_CACHE_FILE, &"a@example.com".to_string());
        // b has only the email half — the uuid anchor hasn't backfilled yet.
        write_profile_cache("b", ACCOUNT_EMAIL_CACHE_FILE, &"b@example.com".to_string());

        let id = |uuid: &str, email: Option<&str>| crate::usage::AccountIdentity {
            uuid: uuid.to_string(),
            email: email.map(str::to_string),
        };
        assert_eq!(
            crate::actions::account_owner(&cfg, &id("uuid-1", Some("x@y")), "new"),
            Some("a".to_string()),
            "uuid match names the holder",
        );
        assert_eq!(
            crate::actions::account_owner(&cfg, &id("uuid-9", Some("B@EXAMPLE.com")), "new"),
            Some("b".to_string()),
            "email-only match is case-insensitive",
        );
        assert_eq!(
            crate::actions::account_owner(&cfg, &id("uuid-1", Some("a@example.com")), "a"),
            None,
            "re-logging the holder itself is a refresh, not a duplicate",
        );
        assert_eq!(
            crate::actions::account_owner(&cfg, &id("uuid-9", Some("c@example.com")), "new"),
            None,
            "an unheld account is storable",
        );
        // Double-hold recovery: with BOTH profiles anomalously anchored to the
        // same account, re-minting it for either one is a refresh — refusing
        // would wedge the recovery in both directions (each names the other).
        write_profile_cache("b", ACCOUNT_ID_CACHE_FILE, &"uuid-1".to_string());
        assert_eq!(
            crate::actions::account_owner(&cfg, &id("uuid-1", None), "a"),
            None,
            "refreshing one half of an existing double-hold is allowed",
        );
        assert_eq!(
            crate::actions::account_owner(&cfg, &id("uuid-1", None), "new"),
            Some("a".to_string()),
            "a THIRD profile minting the doubled account is still refused",
        );
    }
}

// CDX-1 re-verify gap: the endpoint editor is a claude-shaped credential
// writer too — it must refuse a codex target (writing base_url/api_key would
// set `provider` and re-enter the excluded fetch legs).
#[test]
fn edit_profile_endpoint_refuses_a_codex_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut cdx = crate::testutil::blank_profile("cdx-a");
    cdx.harness = crate::profile::Harness::Codex;
    let mut cfg = AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![cdx],
    };
    let err = edit_profile_endpoint(
        &mut cfg,
        "cdx-a",
        Some("https://api.anthropic.com".into()),
        Some("sk-claude".into()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("codex profile"), "{err}");
    let p = cfg.find("cdx-a").unwrap();
    assert!(p.base_url.is_none() && p.api_key.is_none() && p.provider.is_none());
}
