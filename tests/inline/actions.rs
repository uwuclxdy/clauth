#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Per-account custom env editor: collision classification + `edit_profile_env`
//! persistence and the strip-removed-keys-on-active behaviour.

use super::*;
use crate::profile::AppState;
use crate::testutil::HomeSandbox;

/// The rotation guard every account mutation takes. Uncontended inside a
/// sandbox, so this is the fixture spelling of "no rotation is in flight" — the
/// contended direction is what the two refusal tests below drive.
fn rotation_guard(name: &str) -> crate::runtime::RotationGuard {
    rotation_guard_for_mutation(&crate::profile::ProfileName::from(name))
        .expect("uncontended rotation lock")
}

/// A locked handle on `name`'s rotation lock from a separate fd, standing in for
/// another process mid-rotation (`flock(2)` binds to the open file description,
/// so this genuinely contends). Creates the locks directory the way
/// `try_acquire` does, since a real holder made it on its way in.
fn hold_rotation_lock(name: &str) -> std::fs::File {
    let path = crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from(name))
        .expect("rotation lock path");
    crate::profile::mkdir_700(path.parent().expect("lock parent")).expect("locks dir");
    let holder = crate::profile::open_state_file(&path).expect("open holder handle");
    holder.lock().expect("hold the rotation lock");
    holder
}

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
    config.state.profiles = vec!["cl-ax".into(), "xfx".into()];
    config.state.active_profile = Some("cl-ax".into());
    crate::profile::save_app_state(&config.state).expect("persist state");

    // Must NOT bail — the live file is the active account's captured mirror.
    switch_profile(&mut config, &crate::profile::ProfileName::from("xfx"))
        .expect("switch replaces the active-account mirror");

    assert!(config.is_active(&crate::profile::ProfileName::from("xfx")));
    assert_eq!(
        crate::claude::classify_credentials_link(&crate::profile::ProfileName::from("xfx"))
            .expect("classify"),
        crate::claude::LinkState::LinkedTo,
        "after the switch the live path resolves to xfx's stored creds",
    );
}

/// `switch_profile` to a name with no profile must bail BEFORE any side
/// effect. Pre-fix the existence check lived in `finish_switch` — LAST in the
/// sequence — so `force_link_profile_credentials` had already torn down the
/// live `.credentials.json` for a ghost target (a profile deleted by
/// `clauth delete` while a queued auto-switch — e.g. a daemon's pending
/// switch — MCP switch, or CLI switch still held its name), destroying the
/// live login even though the switch itself failed.
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

    let err = switch_profile(&mut config, &crate::profile::ProfileName::from("ghost"))
        .expect_err("ghost must bail");
    assert!(
        err.to_string().contains("not found"),
        "bail names the cause, got: {err}"
    );
    assert!(
        config.is_active(&crate::profile::ProfileName::from("keeper")),
        "active unchanged"
    );
    assert!(live_path.exists(), "the live credentials file survives");
    let stored = crate::profile::profile_dir(&crate::profile::ProfileName::from("keeper"))
        .unwrap()
        .join("credentials.json");
    assert!(stored.exists(), "keeper's stored credentials survive");
}

/// The fresh-membership gate in `ensure_switch_target_ok` (not the in-memory
/// `find`) is what bounces a target deleted on disk while a caller holds a
/// stale config. It runs BEFORE `force_link_profile_credentials`, so the live
/// slot is never torn down for a ghost — the failure mode a `finish_switch`-only
/// gate would leave open.
#[test]
fn switch_profile_refuses_a_target_deleted_on_disk() {
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
        save_profile(&p).expect("save profile");
        p
    };
    let active = mk("keeper", "keeper-access");
    let victim = mk("victim", "victim-access");

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(active.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let config = AppConfig {
        state: AppState {
            active_profile: Some("keeper".into()),
            profiles: vec!["keeper".into(), "victim".into()],
            ..AppState::default()
        },
        profiles: vec![active, victim],
    };
    save_app_state(&config.state).expect("persist state");

    // The leg's snapshot predates the delete.
    let mut stale = config.clone();

    // CLI account mutation: delete victim out from under the stale snapshot.
    // `victim` is not active on the delete config, so the live file survives and
    // the gate under test is what refuses, not a missing live slot.
    let mut disk = crate::profile::load_config().expect("load disk config");
    disk.state.active_profile = None;
    let guard = rotation_guard("victim");
    delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("victim"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    let err = switch_profile(&mut stale, &crate::profile::ProfileName::from("victim"))
        .expect_err("a deleted target must be refused");
    assert_eq!(err.to_string(), "profile 'victim' not found");
    assert!(
        stale.is_active(&crate::profile::ProfileName::from("keeper")),
        "the active profile is unchanged"
    );
    assert!(
        live_path.exists(),
        "the live credentials file must survive (the gate fires before force-link)"
    );
    assert!(
        !profile_dir(&crate::profile::ProfileName::from("victim"))
            .expect("dir")
            .exists(),
        "the deleted target's directory must stay deleted"
    );
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
    crate::profile::save_app_state(&config.state).expect("persist state");

    let err = switch_profile(&mut config, &crate::profile::ProfileName::from("target"))
        .expect_err("a disabled target must be refused");
    assert_eq!(
        err.to_string(),
        "'target': account is disabled, run `clauth enable target`"
    );
    assert!(
        config.is_active(&crate::profile::ProfileName::from("active")),
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
    crate::profile::save_app_state(&config.state).expect("persist state");

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(
        action,
        Some(SwitchAction::To("b".to_string())),
        "a dead active with stale-headroom usage must still be walked away from"
    );
    assert!(config.is_active(&crate::profile::ProfileName::from("b")));
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
    crate::profile::save_app_state(&config.state).expect("persist state");

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(
        action,
        Some(SwitchAction::To("b".to_string())),
        "a healthy active with a spent per-model week must hop to the clear member"
    );
    assert!(config.is_active(&crate::profile::ProfileName::from("b")));
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
        config.is_active(&crate::profile::ProfileName::from("a")),
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
    assert!(config.is_active(&crate::profile::ProfileName::from("a")));
}

#[test]
fn edit_profile_env_persists_to_config_toml() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    let mut env = BTreeMap::new();
    env.insert("FOO".to_string(), "bar".to_string());
    edit_profile_env(&mut config, &crate::profile::ProfileName::from("acct"), env)
        .expect("set env");

    assert_eq!(
        config
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .env
            .get("FOO"),
        Some(&"bar".to_string())
    );
    let toml = std::fs::read_to_string(
        profile_dir(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .join("config.toml"),
    )
    .expect("config.toml written");
    assert!(
        toml.contains("FOO"),
        "custom env key persisted to config.toml"
    );

    // Clearing the map persists too.
    edit_profile_env(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        BTreeMap::new(),
    )
    .expect("clear env");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .env
            .is_empty()
    );
}

#[test]
fn edit_profile_env_strips_removed_keys_from_live_settings_when_active() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    config.state.active_profile = Some("acct".into());

    let mut env = BTreeMap::new();
    env.insert("KEEP".to_string(), "1".to_string());
    env.insert("DROP".to_string(), "2".to_string());
    edit_profile_env(&mut config, &crate::profile::ProfileName::from("acct"), env)
        .expect("write both");
    let live = crate::claude::claude_settings_env_keys().expect("read settings");
    assert!(live.contains(&"KEEP".to_string()) && live.contains(&"DROP".to_string()));

    // Removing DROP must strip it from the live settings.json, not leak it.
    let mut env2 = BTreeMap::new();
    env2.insert("KEEP".to_string(), "1".to_string());
    edit_profile_env(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        env2,
    )
    .expect("drop one");
    let live = crate::claude::claude_settings_env_keys().expect("read settings");
    assert!(live.contains(&"KEEP".to_string()));
    assert!(
        !live.contains(&"DROP".to_string()),
        "a removed key is stripped from the live settings on re-apply"
    );
}

// ── set_profile_default_model (`clauth login --model`, &crate::profile::ProfileName::from(the create-form row)) ──
// (the ensure_login_profile tests were dropped with the fn — `clauth login` now
//  mints tokens via the browser flow and captures a profile, rather than
//  pre-creating a blank one; `--model` is applied to the captured profile.)

#[test]
fn set_profile_default_model_persists_to_config_toml() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    set_profile_default_model(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        "opus",
    )
    .expect("set model");

    assert_eq!(
        config
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .models
            .default
            .as_deref(),
        Some("opus")
    );
    let toml = std::fs::read_to_string(
        profile_dir(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .join("config.toml"),
    )
    .expect("config.toml written");
    assert!(toml.contains("opus"), "model persisted to config.toml");
}

#[test]
fn set_profile_default_model_preserves_alias_overrides() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    edit_profile_model(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        ModelSettings {
            opus: Some("claude-opus-4-8".to_string()),
            ..ModelSettings::default()
        },
    )
    .expect("seed opus alias");

    set_profile_default_model(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        "sonnet",
    )
    .expect("set default");

    let profile = config
        .find(&crate::profile::ProfileName::from("acct"))
        .unwrap();
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
    set_profile_default_model(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        "opus",
    )
    .expect("set model");
    set_profile_default_model(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        "   ",
    )
    .expect("clear model");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .models
            .default
            .is_none(),
        "blank input clears the default, mirroring the Setup tab's ⏎ commit"
    );
}

#[test]
fn edit_profile_preset_writes_endpoint_and_models_in_one_shot() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();

    // Seed an api key + an old model block so we can prove both are preserved
    // (the key entirely, and the model block replaced — not merged).
    config.profiles[0].api_key = Some("sk-secret".to_string());
    config.profiles[0].models.opus = Some("old-opus".to_string());
    config.profiles[0].models.default = Some("old-default".to_string());

    edit_profile_preset(
        &mut config,
        &crate::profile::ProfileName::from("acct"),
        Some("https://api.deepseek.com/anthropic".to_string()),
        ModelSettings {
            default: Some("deepseek-chat".to_string()),
            ..ModelSettings::default()
        },
    )
    .expect("preset applied");

    let profile = config
        .find(&crate::profile::ProfileName::from("acct"))
        .unwrap();
    assert_eq!(
        profile.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "endpoint landed"
    );
    assert_eq!(
        profile.models.default.as_deref(),
        Some("deepseek-chat"),
        "default model landed"
    );
    assert_eq!(
        profile.models.opus, None,
        "the model block is replaced wholesale, not merged"
    );
    assert_eq!(
        profile.api_key.as_deref(),
        Some("sk-secret"),
        "the account's own key survives a preset stamp"
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

    let history_path = profile_dir(&crate::profile::ProfileName::from("acme"))
        .unwrap()
        .join("usage_history.jsonl");
    std::fs::write(&history_path, b"{\"ts\":1}\n").expect("seed usage history");

    // Seed the transient fetch-state caches the overwrite must drop.
    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from("acme"),
            file,
            &"stale",
        );
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
        account_uuid: None,
    };

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        snapshot,
    )
    .expect("overwrite in place");

    assert_eq!(
        config.profiles.len(),
        3,
        "no duplicate entry from a blind append"
    );
    let acme = config
        .find(&crate::profile::ProfileName::from("acme"))
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
        let path = crate::profile_cache::profile_cache_path(
            &crate::profile::ProfileName::from("acme"),
            file,
        )
        .unwrap();
        assert!(
            !path.exists(),
            "{file} must be dropped — it describes the old account"
        );
    }
}

/// "Preserve key on reauth" (owner ruling, 2026-08-30): a browser reauth's
/// snapshot carries the minted tokens and nothing else (`run_oauth_browser`),
/// so the uniform replace stripped a third-party profile's endpoint and key —
/// a login about the OAuth chain deleting the working api-key credential. A
/// field the snapshot omits keeps the stored one on a provider-set profile.
/// The console rule is unchanged: it keys on the EFFECTIVE provider, so an
/// Alibaba reauth keeps its session and a non-Alibaba one still clears it.
#[test]
fn browser_reauth_on_a_third_party_profile_keeps_its_endpoint_and_key() {
    let _home = HomeSandbox::new();

    fn pair(access: &str) -> ClaudeCredentials {
        ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }
    let console = || crate::profile::ConsoleCredential {
        token: "console-token".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    };

    let mut ds = Profile::new(
        "ds-hybrid".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-old".to_string()),
    );
    ds.credentials = Some(pair("old-access"));
    ds.console = Some(console());
    let mut qwen = Profile::new(
        "qwen-hybrid".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com".to_string()),
        Some("sk-sp-old".to_string()),
    );
    qwen.credentials = Some(pair("old-access"));
    qwen.console = Some(console());
    save_profile(&ds).expect("save ds");
    save_profile(&qwen).expect("save qwen");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec![ds.name.clone(), qwen.name.clone()],
            ..AppState::default()
        },
        profiles: vec![ds, qwen],
    };

    for (name, access) in [("ds-hybrid", "ds-new"), ("qwen-hybrid", "qwen-new")] {
        let snapshot = CaptureSnapshot {
            credentials: Some(pair(access)),
            base_url: None,
            api_key: None,
            account_uuid: None,
        };
        overwrite_captured_profile(
            &mut config,
            &crate::profile::ProfileName::from(name),
            snapshot,
        )
        .expect("reauth");
    }

    let ds = config
        .find(&crate::profile::ProfileName::from("ds-hybrid"))
        .expect("profile");
    assert_eq!(
        ds.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "a snapshot that omits the endpoint keeps the stored one"
    );
    assert_eq!(
        ds.api_key.as_deref(),
        Some("sk-old"),
        "a snapshot that omits the key keeps the stored one"
    );
    assert_eq!(
        ds.provider,
        Some(crate::providers::Provider::DeepSeek),
        "the provider is re-derived off the PRESERVED endpoint"
    );
    assert_eq!(
        ds.access_token(),
        Some("ds-new"),
        "the credential set is still replaced"
    );
    assert!(
        ds.console.is_none(),
        "non-Alibaba keeps the console-clearing rule"
    );

    let qwen = config
        .find(&crate::profile::ProfileName::from("qwen-hybrid"))
        .expect("profile");
    assert_eq!(
        qwen.base_url.as_deref(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com")
    );
    assert_eq!(qwen.api_key.as_deref(), Some("sk-sp-old"));
    assert_eq!(
        qwen.provider,
        Some(crate::providers::Provider::Alibaba),
        "the preserved endpoint keeps the Alibaba identity"
    );
    assert!(
        qwen.console.is_some(),
        "an Alibaba reauth keeps its console session — the clear keys on the \
         effective provider, and preservation does not change that rule"
    );
    assert_eq!(qwen.access_token(), Some("qwen-new"));
}

/// A switch that follows `switch_off` has no outgoing marker to read, and a
/// cleared `active_profile` is clauth's record rather than a statement about
/// `settings.json` — `switch_off` never touches the file. Stripping nothing
/// there leaves the departed account's `[env]` entries live while the same
/// write repoints the endpoint and `apiKeyHelper` at the incoming account.
/// Reachable unattended: the fallback walk switches off, then switches to.
///
/// `ANTHROPIC_AUTH_TOKEN` cannot show this — `build_claude_settings_json`
/// clears that one on every write, whatever the strip list says.
#[test]
fn a_switch_after_a_switch_off_does_not_inherit_the_departed_accounts_env() {
    let _home = HomeSandbox::new();

    let mut departing = Profile::new("departing".to_string(), None, None);
    departing.env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "departing-token".to_string(),
    );
    departing.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-departing".to_string(),
            refresh_token: Some("rt-departing".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    let mut incoming = Profile::new(
        "incoming".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-incoming".to_string()),
    );
    incoming.credentials = None;
    save_profile(&departing).expect("save departing");
    save_profile(&incoming).expect("save incoming");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["departing".into(), "incoming".into()],
            active_profile: Some("departing".into()),
            ..AppState::default()
        },
        profiles: vec![departing, incoming],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    #[allow(clippy::expect_used, reason = "test")]
    let departing_ref = config
        .find(&crate::profile::ProfileName::from("departing"))
        .expect("profile");
    crate::claude::apply_profile_to_claude_settings(departing_ref, &[])
        .expect("seed the departing account's env into the live settings");

    switch_off(&mut config).expect("switch off");
    assert_eq!(
        config.state.active_profile, None,
        "fixture: the marker must be cleared, which is what the switch then reads"
    );

    switch_profile(&mut config, &crate::profile::ProfileName::from("incoming"))
        .expect("switch to the incoming account");

    let settings = crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    assert!(
        !settings.contains("departing-token"),
        "the departed account's env entry must not survive under the incoming \
         account's endpoint: {settings}"
    );
}

/// "Widen to any endpoint" (owner ruling, 2026-08-30): the preserve keys on a
/// stored endpoint plus working inference auth, never on a RECOGNISED provider
/// — most endpoints in use (litellm, LMStudio, ollama, a router) resolve to
/// `provider: None` and lose exactly as much. An endpoint that is empty once
/// trimmed is no endpoint, spelled the way `effective_base_url` spells the
/// same emptiness test for the api key beside it.
#[test]
fn browser_reauth_keeps_a_generic_endpoint_and_key() {
    let _home = HomeSandbox::new();

    fn oauth_pair(access: &str) -> ClaudeCredentials {
        ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    let mut generic = Profile::new(
        "litellm".to_string(),
        Some("http://127.0.0.1:4000".to_string()),
        Some("sk-generic".to_string()),
    );
    generic.credentials = Some(oauth_pair("old-access"));
    let mut blank_endpoint = Profile::new(
        "blank-endpoint".to_string(),
        Some("   ".to_string()),
        Some("sk-generic".to_string()),
    );
    blank_endpoint.credentials = Some(oauth_pair("old-access"));
    save_profile(&generic).expect("save generic");
    save_profile(&blank_endpoint).expect("save blank");
    assert_eq!(
        config_provider_of(&generic),
        None,
        "fixture: the endpoint must be one clauth has no provider for",
    );

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["litellm".into(), "blank-endpoint".into()],
            ..AppState::default()
        },
        profiles: vec![generic, blank_endpoint],
    };

    for name in ["litellm", "blank-endpoint"] {
        overwrite_captured_profile(
            &mut config,
            &crate::profile::ProfileName::from(name),
            CaptureSnapshot {
                credentials: Some(oauth_pair("new-access")),
                base_url: None,
                api_key: None,
                account_uuid: None,
            },
        )
        .expect("reauth");
    }

    let generic = config
        .find(&crate::profile::ProfileName::from("litellm"))
        .expect("profile");
    assert_eq!(
        generic.base_url.as_deref(),
        Some("http://127.0.0.1:4000"),
        "an unrecognised endpoint is preserved like a recognised one"
    );
    assert_eq!(generic.api_key.as_deref(), Some("sk-generic"));
    assert_eq!(generic.provider, None, "and stays unrecognised");
    assert_eq!(generic.access_token(), Some("new-access"));

    let blank = config
        .find(&crate::profile::ProfileName::from("blank-endpoint"))
        .expect("profile");
    assert_eq!(
        blank.base_url, None,
        "a whitespace-only endpoint is no endpoint to preserve"
    );
    assert_eq!(
        blank.api_key, None,
        "and its key goes with it, rather than being kept against nothing"
    );
}

/// The preserve arm and the load boundary must count the same credential
/// shapes. The preserve gate (`has_own_inference_endpoint`) counts an
/// `[env] ANTHROPIC_AUTH_TOKEN` as working inference auth; the load
/// boundary's `effective_base_url` once counted only the api key, so an
/// env-token profile kept its endpoint through the reauth and then had it
/// nulled at the next `load_profile` — the freshly stored pair read as the
/// bearer-leak shape. The ruled preserve outcome must hold DURABLY: this
/// drives the whole shape end to end, preserve result first, then the load
/// that must agree with it. Both env spellings `has_inference_auth` counts
/// run the same drive, so the preserve arm's env half is pinned for each
/// key, not just the one the defect was filed on.
#[test]
fn an_env_token_profiles_endpoint_survives_reauth_and_the_next_load() {
    let _home = HomeSandbox::new();

    for (name, env_key) in [
        ("env-token", "ANTHROPIC_AUTH_TOKEN"),
        ("env-api-key", "ANTHROPIC_API_KEY"),
    ] {
        let mut profile = Profile::new(
            name.to_string(),
            Some("http://127.0.0.1:4000".to_string()),
            None,
        );
        profile
            .env
            .insert(env_key.to_string(), "env-bearer".to_string());
        save_profile(&profile).expect("save profile");
        assert_eq!(
            config_provider_of(&profile),
            None,
            "fixture: the endpoint must be one clauth has no provider for, so \
             the preserve gate cannot pass through provider recognition"
        );

        let mut config = AppConfig {
            state: AppState {
                profiles: vec![name.into()],
                ..AppState::default()
            },
            profiles: vec![profile],
        };

        // A browser reauth snapshot: the minted pair, nothing else. The
        // profile's inference auth is the env entry, so the endpoint must
        // survive the overwrite (`has_own_inference_endpoint`).
        overwrite_captured_profile(
            &mut config,
            &crate::profile::ProfileName::from(name),
            CaptureSnapshot {
                credentials: Some(ClaudeCredentials {
                    claude_ai_oauth: Some(crate::profile::OAuthToken {
                        access_token: "new-access".to_string(),
                        refresh_token: Some("new-refresh".to_string()),
                        expires_at: None,
                        scopes: None,
                        subscription_type: None,
                    }),
                }),
                base_url: None,
                api_key: None,
                account_uuid: None,
            },
        )
        .expect("reauth");

        let after_reauth = config
            .find(&crate::profile::ProfileName::from(name))
            .expect("profile");
        assert_eq!(
            after_reauth.base_url.as_deref(),
            Some("http://127.0.0.1:4000"),
            "[{env_key}] the env entry is working inference auth, so the \
             preserve arm keeps the endpoint"
        );

        // The reauth stored a pair, so the next load reads pair + endpoint +
        // no api key — and must still keep the endpoint, because the env
        // entry, not the bearer, is what the spawned claude authenticates
        // with.
        let loaded = crate::profile::load_profile(&crate::profile::ProfileName::from(name))
            .expect("load_profile");
        assert_eq!(
            loaded.base_url.as_deref(),
            Some("http://127.0.0.1:4000"),
            "[{env_key}] the load boundary counts the env entry too: the \
             preserved endpoint must survive the next load_profile"
        );
        assert_eq!(
            loaded.env.get(env_key).map(String::as_str),
            Some("env-bearer"),
            "the surviving credential shape is the env entry, not a key"
        );
        assert_eq!(loaded.api_key.as_deref(), None);
    }
}

/// The provider a profile's endpoint resolves to, for a fixture control.
fn config_provider_of(profile: &Profile) -> Option<crate::providers::Provider> {
    profile
        .base_url
        .as_deref()
        .and_then(crate::providers::Provider::from_base_url)
}

/// The auto-activate arm writes the settings too. It never did before, and
/// never had to: a browser snapshot cleared the endpoint on its way through,
/// so there was nothing to write. With the endpoint preserved, an arm that
/// makes this profile active while leaving `settings.json` endpoint-less
/// routes the live session at Anthropic under a profile that says otherwise.
#[test]
fn the_auto_activate_arm_writes_a_preserved_endpoint_into_the_live_settings() {
    let _home = HomeSandbox::new();

    let mut ds = Profile::new(
        "ds-autoactivate".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-live".to_string()),
    );
    ds.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&ds).expect("save ds");

    // A DEPARTING account whose env entry is still sitting in the live
    // settings: a cleared `active_profile` is clauth's record, not a statement
    // about the file (`switch_off` and the TUI divergence arm clear the marker
    // without touching it), so this is the state the activation write lands on.
    // NOT `ANTHROPIC_AUTH_TOKEN`: `build_claude_settings_json` clears that one
    // unconditionally, so it is the single key that cannot leak. Everything
    // else in an `[env]` block survives on exactly the strip list it is handed.
    let mut departed = Profile::new("departed".to_string(), None, None);
    departed.env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "departing-token".to_string(),
    );
    save_profile(&departed).expect("save departed");
    crate::claude::apply_profile_to_claude_settings(&departed, &[])
        .expect("seed the departing account's env into the live settings");

    // No active profile at all: the shape left behind by deleting the active
    // one, which is what makes the next reauth auto-activate.
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["ds-autoactivate".into(), "departed".into()],
            active_profile: None,
            ..AppState::default()
        },
        profiles: vec![ds, departed],
    };

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ds-autoactivate"),
        CaptureSnapshot {
            credentials: Some(ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "new-access".to_string(),
                    refresh_token: Some("new-refresh".to_string()),
                    expires_at: None,
                    scopes: None,
                    subscription_type: None,
                }),
            }),
            base_url: None,
            api_key: None,
            account_uuid: None,
        },
    )
    .expect("reauth");

    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("ds-autoactivate"),
        "fixture: the arm under test is the auto-activating one",
    );
    let settings = crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    assert!(
        !settings.contains("departing-token"),
        "a departing account's env entry must not survive under the incoming \
         account's endpoint: {settings}"
    );
    let live = crate::claude::read_claude_endpoint_config().expect("read live endpoint");
    assert_eq!(
        live.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "the profile it just made active must route where the profile says"
    );
    assert_eq!(live.api_key.as_deref(), Some("sk-live"));
}

/// `capture_into_profile` auto-activates on a live login and wrote NO settings
/// while doing it: after a `switch_off` the departed account's `[env]` entries
/// are still sitting in the live file (the marker was cleared, never the
/// file), so the freshly captured account activated in front of a stale key.
/// The auto-activate arm must write, stripping the departed account's keys.
///
/// NOT `ANTHROPIC_AUTH_TOKEN`: `build_claude_settings_json` clears that one
/// unconditionally, so it is the single key that cannot leak.
#[test]
fn a_fresh_capture_after_a_switch_off_strips_the_departed_accounts_env() {
    let _home = HomeSandbox::new();

    let mut departing = Profile::new("departing".to_string(), None, None);
    departing.env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "departing-token".to_string(),
    );
    departing.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-departing".to_string(),
            refresh_token: Some("rt-departing".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&departing).expect("save departing");
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["departing".into()],
            active_profile: Some("departing".into()),
            ..AppState::default()
        },
        profiles: vec![departing],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let departing_ref = config
        .find(&crate::profile::ProfileName::from("departing"))
        .expect("profile");
    crate::claude::apply_profile_to_claude_settings(departing_ref, &[])
        .expect("seed the departing account's env into the live settings");

    switch_off(&mut config).expect("switch off");
    assert_eq!(
        config.state.active_profile, None,
        "fixture: the marker must be cleared, which is what the capture then reads"
    );

    capture_into_profile(
        &mut config,
        "incoming".to_string(),
        login_snapshot("rt-incoming", None),
    )
    .expect("capture the incoming account");

    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("incoming"),
        "fixture: the arm under test is the auto-activating one"
    );
    let settings = crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    assert!(
        !settings.contains("departing-token"),
        "a departed account's env entry must not survive under the incoming \
         account: {settings}"
    );
}

/// The TUI's create-account commit (`create_profile_from_login`) auto-activates
/// exactly like `capture_into_profile`, and wrote NO settings either — the same
/// reach: `switch_off`, then create a new account from the Setup tab.
#[test]
fn a_tui_create_account_after_a_switch_off_strips_the_departed_accounts_env() {
    let _home = HomeSandbox::new();

    let mut departing = Profile::new("departing".to_string(), None, None);
    departing.env.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "departing-token".to_string(),
    );
    departing.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-departing".to_string(),
            refresh_token: Some("rt-departing".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&departing).expect("save departing");
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["departing".into()],
            active_profile: Some("departing".into()),
            ..AppState::default()
        },
        profiles: vec![departing],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let departing_ref = config
        .find(&crate::profile::ProfileName::from("departing"))
        .expect("profile");
    crate::claude::apply_profile_to_claude_settings(departing_ref, &[])
        .expect("seed the departing account's env into the live settings");

    switch_off(&mut config).expect("switch off");
    assert_eq!(
        config.state.active_profile, None,
        "fixture: the marker must be cleared, which is what the commit then reads"
    );

    create_profile_from_login(
        &mut config,
        "incoming".to_string(),
        None,
        ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-incoming".to_string(),
                refresh_token: Some("rt-incoming".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        },
        None,
    )
    .expect("create account");

    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("incoming"),
        "fixture: the arm under test is the auto-activating one"
    );
    let settings = crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    assert!(
        !settings.contains("departing-token"),
        "a departed account's env entry must not survive under the incoming \
         account: {settings}"
    );
}

/// The preserve is PAIR-WISE: a snapshot carrying one endpoint field but not
/// the other takes both from the snapshot. Per-field fallback would marry one
/// vendor's stored key to another vendor's incoming host and then transmit it
/// there — a live-read snapshot (the divergence adopt) can arrive half-filled.
#[test]
fn a_half_filled_snapshot_never_pairs_a_stored_key_with_a_new_endpoint() {
    let _home = HomeSandbox::new();

    let ds = Profile::new(
        "ds-mixed".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-deepseek".to_string()),
    );
    let other = Profile::new(
        "ds-keyonly".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-deepseek".to_string()),
    );
    save_profile(&ds).expect("save ds");
    save_profile(&other).expect("save other");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["ds-mixed".into(), "ds-keyonly".into()],
            ..AppState::default()
        },
        profiles: vec![ds, other],
    };

    // Endpoint only: the incoming host must not inherit the stored key.
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ds-mixed"),
        CaptureSnapshot {
            credentials: None,
            base_url: Some("https://api.z.ai/api/anthropic".to_string()),
            api_key: None,
            account_uuid: None,
        },
    )
    .expect("endpoint-only capture");
    let mixed = config
        .find(&crate::profile::ProfileName::from("ds-mixed"))
        .expect("profile");
    assert_eq!(
        mixed.base_url.as_deref(),
        Some("https://api.z.ai/api/anthropic")
    );
    assert_eq!(
        mixed.api_key, None,
        "one vendor's key must never be re-paired with another vendor's host"
    );

    // Key only: the stored endpoint must not survive under a new key either.
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ds-keyonly"),
        CaptureSnapshot {
            credentials: None,
            base_url: None,
            api_key: Some("sk-foreign".to_string()),
            account_uuid: None,
        },
    )
    .expect("key-only capture");
    let keyonly = config
        .find(&crate::profile::ProfileName::from("ds-keyonly"))
        .expect("profile");
    assert_eq!(keyonly.api_key.as_deref(), Some("sk-foreign"));
    assert_eq!(
        keyonly.base_url, None,
        "a half-filled snapshot replaces the endpoint set whole"
    );
}

/// The preserve arm stands down with no key behind the endpoint: preserving
/// the `base_url` alone would leave the freshly minted ANTHROPIC bearer
/// pointed at the third-party host, which the `was_active` leg writes into the
/// live settings at once. Reachable through `clear_profile_api_key` (the TUI
/// clears the key and keeps the endpoint) followed by a bare `clauth login`.
#[test]
fn browser_reauth_does_not_keep_an_endpoint_with_no_key_behind_it() {
    let _home = HomeSandbox::new();

    let keyless = Profile::new(
        "ds-keyless".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    );
    save_profile(&keyless).expect("save keyless");
    crate::claude::apply_profile_to_claude_settings(&keyless, &[]).expect("seed live settings");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["ds-keyless".into()],
            active_profile: Some("ds-keyless".into()),
            ..AppState::default()
        },
        profiles: vec![keyless],
    };
    assert!(
        config
            .find(&crate::profile::ProfileName::from("ds-keyless"))
            .is_some_and(|p| p.is_third_party() && !crate::claude::has_inference_auth(p)),
        "fixture: the profile must be third-party with nothing to authenticate with",
    );

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ds-keyless"),
        CaptureSnapshot {
            credentials: Some(ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "anthropic-bearer".to_string(),
                    refresh_token: Some("anthropic-refresh".to_string()),
                    expires_at: None,
                    scopes: None,
                    subscription_type: None,
                }),
            }),
            base_url: None,
            api_key: None,
            account_uuid: None,
        },
    )
    .expect("reauth");

    let profile = config
        .find(&crate::profile::ProfileName::from("ds-keyless"))
        .expect("profile");
    assert_eq!(
        profile.base_url, None,
        "an endpoint with no credential behind it must not survive an OAuth reauth"
    );
    assert_eq!(profile.provider, None);
    let live = crate::claude::read_claude_endpoint_config().expect("read live endpoint");
    assert_eq!(
        live.base_url, None,
        "and the live settings must not route the minted Anthropic bearer to a third-party host"
    );
}

/// The preserve arm through the ACTIVE profile's re-apply leg: `was_active`
/// re-writes `settings.json` from the just-saved record, so a browser reauth on
/// an active third-party profile must leave the live endpoint + key standing
/// rather than stripping the running `claude`'s only inference credential.
#[test]
fn browser_reauth_on_an_active_third_party_profile_keeps_the_live_endpoint() {
    let _home = HomeSandbox::new();

    let mut ds = Profile::new(
        "ds-active".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-live".to_string()),
    );
    ds.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&ds).expect("save ds");
    crate::claude::apply_profile_to_claude_settings(&ds, &[]).expect("seed live settings");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["ds-active".into()],
            active_profile: Some("ds-active".into()),
            ..AppState::default()
        },
        profiles: vec![ds],
    };

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ds-active"),
        CaptureSnapshot {
            credentials: Some(ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "new-access".to_string(),
                    refresh_token: Some("new-refresh".to_string()),
                    expires_at: None,
                    scopes: None,
                    subscription_type: None,
                }),
            }),
            base_url: None,
            api_key: None,
            account_uuid: None,
        },
    )
    .expect("reauth the active profile");

    let live = crate::claude::read_claude_endpoint_config().expect("read live endpoint");
    assert_eq!(
        live.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "the re-apply leg must write the PRESERVED endpoint, not clear it"
    );
    assert_eq!(
        live.api_key.as_deref(),
        Some("sk-live"),
        "a running claude keeps the key its inference authenticates with"
    );
}

/// The uniform replace survives everywhere the preserve arm does not apply:
/// a profile with NO stored endpoint has its stray key replaced by a browser
/// snapshot as before, and an api-mode snapshot — which carries both endpoint
/// fields — still replaces the endpoint set on an endpoint profile.
#[test]
fn overwrite_still_replaces_the_endpoint_set_outside_the_preserve_arm() {
    let _home = HomeSandbox::new();

    let mut plain = Profile::new(
        "plain-oauth".to_string(),
        None,
        Some("stray-key".to_string()),
    );
    plain.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    let ds = Profile::new(
        "ds-keyed".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-old".to_string()),
    );
    save_profile(&plain).expect("save plain");
    save_profile(&ds).expect("save ds");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec![plain.name.clone(), ds.name.clone()],
            ..AppState::default()
        },
        profiles: vec![plain, ds],
    };

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("plain-oauth"),
        CaptureSnapshot {
            credentials: Some(ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "fresh".to_string(),
                    refresh_token: Some("fresh-refresh".to_string()),
                    expires_at: None,
                    scopes: None,
                    subscription_type: None,
                }),
            }),
            base_url: None,
            api_key: None,
            account_uuid: None,
        },
    )
    .expect("reauth");
    let plain = config
        .find(&crate::profile::ProfileName::from("plain-oauth"))
        .expect("profile");
    assert_eq!(
        plain.api_key, None,
        "off the preserve arm a browser snapshot still clears the fields it omits"
    );
    assert_eq!(plain.base_url, None);

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ds-keyed"),
        CaptureSnapshot {
            credentials: None,
            base_url: Some("https://api.z.ai/api/anthropic".to_string()),
            api_key: Some("zai-key".to_string()),
            account_uuid: None,
        },
    )
    .expect("api-mode reauth");
    let ds = config
        .find(&crate::profile::ProfileName::from("ds-keyed"))
        .expect("profile");
    assert_eq!(
        ds.base_url.as_deref(),
        Some("https://api.z.ai/api/anthropic"),
        "an api-mode snapshot carries both fields and replaces them"
    );
    assert_eq!(ds.api_key.as_deref(), Some("zai-key"));
    assert_eq!(
        ds.provider,
        Some(crate::providers::Provider::Zai),
        "the provider follows the replaced endpoint"
    );
    assert_eq!(
        ds.access_token(),
        None,
        "a credentials-less snapshot clears the OAuth pair (only cmd_login's \
         api-mode reauth carries a stored chain through)"
    );
}

/// The chain-preserve must key on api-mode reauth ALONE (owner ruling): a
/// credentials-less endpoint-pair snapshot committed by any other producer is
/// the recapture shape, and its drop of the stored OAuth chain is a
/// deliberate sign-out. `cmd_login`'s api-mode arm carries the stored chain in
/// the snapshot, so `overwrite_captured_profile` itself must keep replacing
/// credentials with exactly what it holds.
#[test]
fn a_credentials_less_recapture_still_drops_the_stored_chain() {
    let _home = HomeSandbox::new();

    let mut acme = Profile::new(
        "acme".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-old".to_string()),
    );
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

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![acme],
    };

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        CaptureSnapshot {
            credentials: None,
            base_url: Some("https://api.z.ai/api/anthropic".to_string()),
            api_key: Some("zai-key".to_string()),
            account_uuid: None,
        },
    )
    .expect("recapture");

    let acme = config
        .find(&crate::profile::ProfileName::from("acme"))
        .expect("profile");
    assert_eq!(
        acme.access_token(),
        None,
        "the recapture is a sign-out: only the api-mode reauth arm preserves a chain"
    );
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

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        login_snapshot("fresh-refresh", None),
    )
    .expect("capture succeeds");

    assert_eq!(
        config.state.active_profile, None,
        "a disabled profile must never be auto-activated, even with no active profile at all"
    );
    assert_eq!(
        config
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .access_token(),
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
    // The cache writes below are gated on the on-disk record, which
    // `save_profile` does not touch; the arm is a live fetch's arm.
    crate::testutil::register_names(&[name]);
    let profile = Profile::new(name.to_string(), None, None);
    save_profile(&profile).expect("save profile");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from(name),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-old-account".to_string(),
    );
    assert!(
        crate::usage::take_profile_fetch(&crate::profile::ProfileName::from(name), false, t0),
        "the first attempt arms the clock"
    );
    assert!(
        !crate::usage::take_profile_fetch(
            &crate::profile::ProfileName::from(name),
            false,
            t0 + 60_000
        ),
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
        &crate::profile::ProfileName::from(name),
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
        account_uuid: account_uuid.map(crate::profile::AccountId::from),
    }
}

#[test]
fn overwrite_captured_profile_anchors_the_account_it_committed() {
    let _home = HomeSandbox::new();
    let target = Profile::new("swap".to_string(), None, None);
    save_profile(&target).expect("save target");
    let mut config = inactive_config(target);
    crate::usage::seed_login_anchor(
        &crate::profile::ProfileName::from("swap"),
        Some(&crate::profile::AccountId::from(
            "uuid-old-account".to_string(),
        )),
    );

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("swap"),
        login_snapshot("new", Some("uuid-new")),
    )
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
fn a_snapshot_with_no_proven_identity_leaves_the_anchor_alone() {
    let _home = HomeSandbox::new();
    // The seed below is a cache write, gated on the on-disk record.
    crate::testutil::register_names(&["unproven"]);
    let target = Profile::new("unproven".to_string(), None, None);
    save_profile(&target).expect("save target");
    let mut config = inactive_config(target);
    crate::usage::seed_login_anchor(
        &crate::profile::ProfileName::from("unproven"),
        Some(&crate::profile::AccountId::from(
            "uuid-existing".to_string(),
        )),
    );

    // `capture_snapshot()` reads live creds off disk and proves no identity; a
    // failed login probe reports none either. Neither may mint OR clear an anchor
    // — a `None` stays the silent no-op it has always been.
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("unproven"),
        login_snapshot("new", None),
    )
    .expect("overwrite in place");

    assert_eq!(
        anchor_of("unproven").as_deref(),
        Some("uuid-existing"),
        "an unproven swap must not clear a live anchor"
    );
}

#[test]
fn overwrite_captured_profile_expires_the_profile_ttl_clock() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-swap", t0));

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("ttl-swap"),
        CaptureSnapshot {
            credentials: None,
            base_url: Some("https://api.example.com".to_string()),
            api_key: Some("new-api-key".to_string()),
            account_uuid: None,
        },
    )
    .expect("overwrite in place");

    assert!(
        crate::usage::take_profile_fetch(
            &crate::profile::ProfileName::from("ttl-swap"),
            false,
            t0 + 120_000
        ),
        "the swapped-in account must pull its own /profile now, not up to an hour later"
    );
}

#[test]
fn clear_profile_credentials_expires_the_profile_ttl_clock() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-logout", t0));

    clear_profile_credentials(
        &mut config,
        &crate::profile::ProfileName::from("ttl-logout"),
    )
    .expect("log out");

    assert!(
        crate::usage::take_profile_fetch(
            &crate::profile::ProfileName::from("ttl-logout"),
            false,
            t0 + 120_000
        ),
        "a re-login into the blanked shell must pull its own tier, not the old account's clock"
    );
}

#[test]
fn delete_profile_expires_the_profile_ttl_clock() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-del", t0));

    delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("ttl-del"),
        false,
        &rotation_guard("ttl-del"),
    )
    .expect("delete");

    // `remove_dir_all` took the durable stamp; only the memo could survive here,
    // and it would mute the first /profile of a same-name relogin in this process.
    assert!(
        crate::usage::take_profile_fetch(
            &crate::profile::ProfileName::from("ttl-del"),
            false,
            t0 + 120_000
        ),
        "the memo must not outlive the profile it describes"
    );
}

#[test]
fn rename_profile_expires_the_old_names_ttl_clock_and_carries_the_stamp() {
    let _home = HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    let mut config = inactive_config(armed_ttl_profile("ttl-ren-old", t0));

    rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("ttl-ren-old"),
        &crate::profile::ProfileName::from("ttl-ren-new"),
        &rotation_guard("ttl-ren-old"),
    )
    .expect("rename");

    assert!(
        crate::usage::take_profile_fetch(
            &crate::profile::ProfileName::from("ttl-ren-old"),
            false,
            t0 + 120_000
        ),
        "the old name's memo is stranded over a stamp that moved away — expire it"
    );
    // Same account, same clock: the dir move carried the anchor and the stamp, so
    // the new name inherits the hour rather than paying a fresh /profile for it.
    assert!(
        !crate::usage::take_profile_fetch(
            &crate::profile::ProfileName::from("ttl-ren-new"),
            false,
            t0 + 120_000
        ),
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
        account_uuid: None,
    };
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        snapshot,
    )
    .expect("overwrite");

    assert!(
        !config.is_auth_broken(&crate::profile::ProfileName::from("acme")),
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
    crate::claude::link_profile_credentials(&crate::profile::ProfileName::from("acme"))
        .expect("link acme live");

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
        account_uuid: None,
    };
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        snapshot,
    )
    .expect("overwrite active profile");

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

/// Re-applying live state must not be refusable by the very divergence the
/// overwrite just created. The operator asked for this profile's credentials to
/// be replaced, so a live slot holding the OLD login is the expected end of that
/// operation, never an unresolved re-login to protect: the guarded relink reads
/// any REGULAR live file as foreign and refuses, and nothing downstream can
/// resolve a divergence whose other half was overwritten a few lines earlier.
///
/// A regular live file is CC's own shape after a re-login on any host, and it is
/// what clauth itself leaves on a host whose `create_symlink` degrades to a copy
/// (Windows without `SeCreateSymbolicLinkPrivilege`) — where it is the ONLY
/// shape, so that host could not recapture an active profile at all. Measured
/// there 2026-08-12; this pins the fix without needing that host.
#[test]
fn overwriting_the_active_profile_replaces_a_regular_live_file() {
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

    // A REGULAR file, not clauth's symlink: what CC leaves after a re-login, and
    // what clauth itself writes wherever the OS denies symlinks.
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).expect("mkdir .claude");
    std::fs::write(
        &live_path,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "live-side-login" } }),
        )
        .unwrap(),
    )
    .expect("write live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            fallback_chain: vec!["acme".into()],
            active_profile: Some("acme".into()),
            ..AppState::default()
        },
        profiles: vec![acme],
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
        base_url: None,
        api_key: None,
        account_uuid: None,
    };
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        snapshot,
    )
    .expect("overwrite must re-apply live state over a regular live file");

    assert_eq!(
        crate::profile::read_json_file::<ClaudeCredentials>(&live_path)
            .expect("read live")
            .access_token(),
        Some("new-access"),
        "the live slot must carry the login the overwrite just stored"
    );
}

/// The other arm of the same branch: a recapture that stores NO credentials (a
/// third-party snapshot) must leave a clean absence, not a live slot still
/// serving the account that was just replaced. `overwrite_captured_profile_…_when_active`
/// pins this where the slot is clauth's symlink; this pins it where the slot is
/// a regular file, which is the shape a host that cannot symlink always has and
/// the one the forcing relink changed most.
///
/// Deliberately silent on `mcpOAuth`: the carry no-ops when the snapshot stored
/// no file to carry into, so those logins go with the slot. That is a live
/// defect, and a test asserting it here would pin it.
#[test]
fn overwriting_the_active_profile_with_no_credentials_clears_a_regular_live_file() {
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

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).expect("mkdir .claude");
    std::fs::write(
        &live_path,
        serde_json::to_vec(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "old-access" } }),
        )
        .unwrap(),
    )
    .expect("write live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            active_profile: Some("acme".into()),
            ..AppState::default()
        },
        profiles: vec![acme],
    };

    let snapshot = CaptureSnapshot {
        credentials: None,
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("new-api-key".to_string()),
        account_uuid: None,
    };
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("acme"),
        snapshot,
    )
    .expect("a third-party recapture of the active profile must not be refusable");

    assert!(
        live_path.symlink_metadata().is_err(),
        "the replaced account's login must not stay live once the profile stores none"
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
    let profile = config
        .find(&crate::profile::ProfileName::from("api-acct"))
        .expect("profile present")
        .clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");
    assert_eq!(
        crate::claude::read_claude_endpoint_config()
            .expect("read endpoint")
            .api_key
            .as_deref(),
        Some("sk-secret"),
        "precondition: active api key is wired into settings.json"
    );

    delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("api-acct"),
        false,
        &rotation_guard("api-acct"),
    )
    .expect("delete active api profile");

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

    let err = delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("busy"),
        false,
        &rotation_guard("busy"),
    )
    .expect_err("a live session must block an unforced delete");
    // Exact, and worded off the same `live session` noun as the `disable`
    // sibling below: one predicate (`has_live_session`) refusing in two nouns
    // is what sent an operator looking for two different conditions.
    assert_eq!(
        err.to_string(),
        "'busy' has a live session, pass --force to delete it anyway"
    );
    assert!(
        config
            .find(&crate::profile::ProfileName::from("busy"))
            .is_some(),
        "the refused delete must leave the profile record intact"
    );

    delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("busy"),
        true,
        &rotation_guard("busy"),
    )
    .expect("force overrides the live-session guard");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("busy"))
            .is_none(),
        "force must remove the profile despite the live session"
    );
}

/// A delete landing inside a rotation's HTTP window raced it: both took only
/// the state flock, so nothing serialized them. The delete now takes the
/// rotation lock first and refuses on busy — ahead of the credentials unwire,
/// which is the first write in the closure and the one a late refusal would
/// leave half-applied.
///
/// This is the ROTATION-FIRST direction. Its twin below drives delete-first,
/// which is the ordering that used to unlink the held lock inode.
#[test]
fn delete_refuses_while_a_rotation_holds_the_lock() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(
        &mut config,
        "rotating".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
        None,
    )
    .expect("create api profile");
    // Active, so the delete's unwire leg is armed: an api key in the live
    // settings.json is a write the refusal has to land ahead of.
    config.state.active_profile = Some("rotating".into());
    let profile = config
        .find(&crate::profile::ProfileName::from("rotating"))
        .expect("profile present")
        .clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");

    let holder = hold_rotation_lock("rotating");

    // The caller's own sequence, both production call sites included: guard
    // first, mutation only if it was granted. Composed rather than asserted on
    // the helper alone, so a guard handed out under contention lets the delete
    // run and the untouched-state assertions below are what catch it.
    let outcome = rotation_guard_for_mutation(&crate::profile::ProfileName::from("rotating"))
        .and_then(|rotation| {
            delete_profile(
                &mut config,
                &crate::profile::ProfileName::from("rotating"),
                false,
                &rotation,
            )
        });

    // Untouched-state first, error second: a guard handed out under contention
    // runs the whole delete, and asserting the error first would abort the body
    // here and leave every line below unexercised.
    assert!(
        config
            .find(&crate::profile::ProfileName::from("rotating"))
            .is_some(),
        "the refused delete must leave the profile record intact"
    );
    assert!(
        profile_dir(&crate::profile::ProfileName::from("rotating"))
            .expect("profile dir")
            .exists(),
        "the refused delete must leave the profile directory in place"
    );
    assert_eq!(
        crate::claude::read_claude_endpoint_config()
            .expect("read endpoint")
            .api_key
            .as_deref(),
        Some("sk-secret"),
        "the refusal must land ahead of the unwire — settings.json is untouched"
    );
    assert_eq!(
        outcome
            .expect_err("an in-flight rotation must block the delete")
            .to_string(),
        "'rotating' has a token rotation in progress, retry in a moment"
    );
    // The row's own hazard, stated as a property rather than a story: while the
    // holder is alive, nothing else can hold this lock. Pre-guard the delete had
    // already unlinked the inode by here, so a relogin's acquire minted a second
    // holder against a rotation still spending the old refresh token. Driving
    // production's own acquire also proves the handle above locks the same path.
    assert!(
        crate::runtime::RotationGuard::try_acquire(&crate::profile::ProfileName::from("rotating"))
            .expect("try_acquire")
            .is_none(),
        "a same-name relogin must not mint a second holder while the rotation runs"
    );

    // Direction 2: the refusal is contention, not a permanent gate.
    drop(holder);
    delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("rotating"),
        false,
        &rotation_guard("rotating"),
    )
    .expect("the delete goes through once the rotation releases");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("rotating"))
            .is_none(),
        "a released lock must let the same delete complete"
    );
}

/// The rename half of the same race: `fs::rename` moves the directory holding
/// the lock inode out from under the rotation, whose persist then resolves the
/// profile by NAME and drops the freshly-minted pair — leaving the renamed
/// account on a refresh token the server has already killed.
#[test]
fn rename_refuses_while_a_rotation_holds_the_lock() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "ren-old".to_string(), None, None, None)
        .expect("create profile");

    let holder = hold_rotation_lock("ren-old");

    // Composed the way both production call sites are, for the reason the
    // delete twin states.
    let outcome = rotation_guard_for_mutation(&crate::profile::ProfileName::from("ren-old"))
        .and_then(|rotation| {
            rename_profile(
                &mut config,
                &crate::profile::ProfileName::from("ren-old"),
                &crate::profile::ProfileName::from("ren-new"),
                &rotation,
            )
        });

    // Untouched-state first, for the reason the delete twin states.
    assert!(
        config
            .find(&crate::profile::ProfileName::from("ren-old"))
            .is_some()
            && config
                .find(&crate::profile::ProfileName::from("ren-new"))
                .is_none(),
        "the refused rename must leave the record under its old name"
    );
    assert!(
        profile_dir(&crate::profile::ProfileName::from("ren-old"))
            .expect("old dir")
            .exists()
            && !profile_dir(&crate::profile::ProfileName::from("ren-new"))
                .expect("new dir")
                .exists(),
        "the refused rename must leave the directory where it was"
    );
    assert_eq!(
        outcome
            .expect_err("an in-flight rotation must block the rename")
            .to_string(),
        "'ren-old' has a token rotation in progress, retry in a moment"
    );

    drop(holder);
    rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("ren-old"),
        &crate::profile::ProfileName::from("ren-new"),
        &rotation_guard("ren-old"),
    )
    .expect("the rename goes through once the rotation releases");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("ren-new"))
            .is_some(),
        "a released lock must let the same rename complete"
    );
}

/// A profile held by a live `clauth start` session must not be renamed — the
/// rename moves the whole profile directory, which holds the session's runtime
/// tree, markers and env paths, and nothing rekeys the live-session registry
/// rows. Same predicate and copy as the disable sibling.
#[test]
fn rename_refuses_a_live_session() {
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

    let err = rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("busy"),
        &crate::profile::ProfileName::from("calm"),
        &rotation_guard("busy"),
    )
    .expect_err("a live session must refuse the rename");
    assert_eq!(err.to_string(), "'busy' has a live session, close it first");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("busy"))
            .is_some()
            && config
                .find(&crate::profile::ProfileName::from("calm"))
                .is_none(),
        "the refused rename must leave the record under its old name"
    );
    assert!(
        profile_dir(&crate::profile::ProfileName::from("busy"))
            .expect("old dir")
            .exists(),
        "the refused rename must leave the directory where it was"
    );
}

/// The name check reads the RECORD; a directory can outlive it — a per-profile
/// cache a stale-config fetch leg wrote after the account was deleted re-creates
/// the dir. rename(2) onto that existing non-empty dir fails ENOTEMPTY, which
/// reads as an internal failure; refuse with the actionable shape instead.
#[test]
fn rename_refuses_a_leftover_target_directory() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "src".to_string(), None, None, None).expect("create profile");

    // A leftover: the target's directory exists with a cache file in it, but no
    // record names the target.
    let leftover = profile_dir(&crate::profile::ProfileName::from("taken")).expect("target dir");
    std::fs::create_dir_all(&leftover).expect("mkdir leftover");
    std::fs::write(
        leftover.join(crate::profile_cache::THIRD_PARTY_CACHE_FILE),
        "{}",
    )
    .expect("write cache");

    let err = rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("src"),
        &crate::profile::ProfileName::from("taken"),
        &rotation_guard("src"),
    )
    .expect_err("a leftover target directory must refuse the rename");
    let msg = err.to_string();
    assert!(
        msg.contains("'taken' already has a directory at")
            && msg.contains("with no account behind it"),
        "the refusal must name the leftover and the way out, got: {msg}"
    );
    assert!(
        config
            .find(&crate::profile::ProfileName::from("src"))
            .is_some()
            && config
                .find(&crate::profile::ProfileName::from("taken"))
                .is_none(),
        "the refused rename must leave the record under its old name"
    );
}

/// The other half a leftover-shaped directory can be: the dir under `new` was
/// moved there BY a rename whose record write never landed (a kill or a failing
/// save between the move and `save_app_state`). It holds the profile's own
/// content, so the retry must complete the record rename — the pre-gate
/// self-heal — rather than refuse and send the operator to delete it.
#[test]
fn rename_retry_completes_the_record_when_the_old_dir_is_already_moved() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "mid-src".to_string(), None, None, None)
        .expect("create profile");

    // The crash window: the dir move happened, the record rename did not.
    std::fs::rename(
        profile_dir(&crate::profile::ProfileName::from("mid-src")).expect("old dir"),
        profile_dir(&crate::profile::ProfileName::from("mid-new")).expect("new dir"),
    )
    .expect("simulate the stranded move");

    rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("mid-src"),
        &crate::profile::ProfileName::from("mid-new"),
        &rotation_guard("mid-src"),
    )
    .expect("a stranded dir holding the profile's own content must not refuse");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("mid-src"))
            .is_none()
            && config
                .find(&crate::profile::ProfileName::from("mid-new"))
                .is_some(),
        "the retry completes the record rename instead of refusing"
    );
    assert!(
        profile_dir(&crate::profile::ProfileName::from("mid-new"))
            .expect("new dir")
            .exists(),
        "the moved directory stays put under the name the record now carries"
    );
}

/// The fetch legs hold a stale in-memory config for up to a tick, so a cache
/// write for a name the record no longer carries used to re-create the deleted
/// profile's directory. The writer now skips those names.
#[test]
fn a_cache_write_does_not_resurrect_a_deleted_profiles_directory() {
    let _home = HomeSandbox::new();

    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("ghost"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &serde_json::json!({"is_available": true}),
    );

    assert!(
        !profile_dir(&crate::profile::ProfileName::from("ghost"))
            .expect("ghost dir")
            .exists(),
        "a cache write must not create a directory for a name no record carries"
    );
}

/// DELETE-FIRST, the direction the row's own premise describes and the one the
/// rotation-first twin above cannot reach. The delete used to `remove_dir_all`
/// the directory its own `rotation.lock` sat in, so it unlinked the inode it was
/// holding; an arriving acquire then found no file, was granted a SECOND holder
/// of one profile's lock, and recreated the profile directory to put it in.
///
/// The lock now lives outside the profile tree, so the delete cannot unlink it.
/// This closes the same-version race only: an older clauth still locks the old
/// path, and the two do not serialize against each other.
#[test]
fn a_delete_does_not_release_the_lock_it_is_holding() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "victim".to_string(), None, None, None).expect("create");

    // Bound to a `let`, so the guard outlives the delete the way `cmd_delete`'s
    // does — it drops at end of scope, well after `remove_dir_all`.
    let own = rotation_guard("victim");
    delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("victim"),
        false,
        &own,
    )
    .expect("delete");

    assert!(
        !profile_dir(&crate::profile::ProfileName::from("victim"))
            .expect("dir")
            .exists(),
        "the delete still removes the profile directory"
    );
    // A second THREAD: a fresh rank stack, the way another process arrives. The
    // same-thread form re-enters ROTATION and trips the rank assert instead —
    // `try_lock` runs before `RankGuard::enter`, so it would mint the holder and
    // only then panic, which is evidence nobody can read.
    let minted = std::thread::spawn(|| {
        crate::runtime::RotationGuard::try_acquire(&crate::profile::ProfileName::from("victim"))
            .expect("try_acquire")
            .is_some()
    })
    .join()
    .expect("join");
    assert!(
        !minted,
        "the delete's own guard must still be the only holder of 'victim'"
    );
    assert!(
        !profile_dir(&crate::profile::ProfileName::from("victim"))
            .expect("dir")
            .exists(),
        "and no arriving acquire may resurrect the deleted profile directory"
    );
}

/// The fault arm — the lock cannot be created or opened at all — speaks the
/// same vocabulary as its `oauth` siblings rather than surfacing a raw errno,
/// and keeps the io error underneath so the chain still says which path failed.
#[test]
fn an_unopenable_rotation_lock_refuses_in_the_fault_vocabulary() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "broken".to_string(), None, None, None).expect("create");

    // A regular file where the locks DIRECTORY belongs, so `mkdir_700` fails.
    let lock = crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from("broken"))
        .expect("rotation lock path");
    let locks_dir = lock.parent().expect("lock parent");
    std::fs::create_dir_all(locks_dir.parent().expect("clauth dir")).expect("clauth dir");
    std::fs::write(locks_dir, b"not a directory").expect("occupy the locks dir path");

    let err = match rotation_guard_for_mutation(&crate::profile::ProfileName::from("broken")) {
        Ok(_) => panic!("an unopenable lock must refuse"),
        Err(e) => e,
    };
    assert_eq!(
        err.to_string(),
        crate::format::Transient::new(
            crate::format::Cause::RotationLockUnavailable("broken".to_string()),
            crate::format::Retry::Stated,
        )
        .text(),
        "the fault arm names the fix, in the same words its oauth siblings use"
    );
    assert!(
        err.chain().count() > 1,
        "the io error stays underneath rather than being replaced"
    );
}

/// Re-spelling an account's own name in another case is not a collision with
/// itself — `validate_profile_name` clears it through its `exclude`, so the
/// duplicate guard on `rename_profile` has to carry that exclusion too. A bare
/// "no account resolves to `new`" fires here, since `canonical_name` folds.
#[test]
fn a_case_only_rename_is_not_a_collision_with_itself() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "work".to_string(), None, None, None).expect("create");

    rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("work"),
        &crate::profile::ProfileName::from("Work"),
        &rotation_guard("work"),
    )
    .expect("an account may be re-spelled in another case");

    assert!(
        config
            .find(&crate::profile::ProfileName::from("Work"))
            .is_some(),
        "the new spelling is the one on record"
    );
}

/// Renaming onto a case VARIANT of a different account is the collision the
/// guard exists for: `canonical_name` folds at every resolution site, so
/// `work2` and `WORK2` are one account holding two directories, and which one
/// answers depends on the `profiles` vec's order. A case-exact guard passes this
/// and is why the assert derives its predicate from the folding resolver.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "already names an account")]
fn a_rename_onto_a_case_variant_of_another_account_is_refused() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "work".to_string(), None, None, None).expect("create");
    create_blank_profile(&mut config, "work2".to_string(), None, None, None).expect("create");

    let _ = rename_profile(
        &mut config,
        &crate::profile::ProfileName::from("work"),
        &crate::profile::ProfileName::from("WORK2"),
        &rotation_guard("work"),
    );
}

/// The refusal an operator meets from `clauth delete` / the TUI and the one the
/// scheduler's re-stamp leg logs are ONE condition, so they are one sentence.
/// Derived from both sides rather than spelled twice: the literal lives in the
/// `format` copy table alone.
#[test]
fn the_mutation_refusal_matches_the_rotation_lock_held_copy() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(&mut config, "shared".to_string(), None, None, None).expect("create");

    // Bound, not discarded: `let _ =` would drop the handle and release the
    // flock before the refusal under test is even attempted.
    let _holder = hold_rotation_lock("shared");

    // `RotationGuard` is not `Debug`, so match rather than `expect_err`.
    let refusal = match rotation_guard_for_mutation(&crate::profile::ProfileName::from("shared")) {
        Ok(_) => panic!("a held rotation lock must refuse"),
        Err(e) => e,
    };
    assert_eq!(
        refusal.to_string(),
        crate::format::Transient::new(
            crate::format::Cause::RotationLockHeld("shared".to_string()),
            crate::format::Retry::Stated,
        )
        .text(),
        "one condition reads as one condition on every surface"
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

    let err = disable_profile(&mut config, &crate::profile::ProfileName::from("acme"))
        .expect_err("active profile must be refused");
    assert_eq!(
        err.to_string(),
        "'acme' is the active account, switch away first"
    );
    assert!(
        !config
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .is_disabled(),
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

    let err = disable_profile(&mut config, &crate::profile::ProfileName::from("busy"))
        .expect_err("a live session must be refused");
    assert_eq!(err.to_string(), "'busy' has a live session, close it first");
    assert!(
        !config
            .find(&crate::profile::ProfileName::from("busy"))
            .unwrap()
            .is_disabled(),
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

    let changed = disable_profile(&mut config, &crate::profile::ProfileName::from("acme"))
        .expect("disable succeeds");
    assert!(changed, "a fresh disable must report a real change");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .is_disabled()
    );

    let reloaded = crate::profile::load_config().expect("reload from disk");
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .is_disabled(),
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
    disable_profile(&mut config, &crate::profile::ProfileName::from("acme"))
        .expect("first disable");

    let changed = disable_profile(&mut config, &crate::profile::ProfileName::from("acme"))
        .expect("re-disable is not an error");
    assert!(!changed, "a no-op disable must report no change");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .is_disabled()
    );
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

    let config_path =
        crate::profile::profile_subpath(&crate::profile::ProfileName::from("acme"), "config.toml")
            .unwrap();
    let creds_path = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from("acme"),
        "credentials.json",
    )
    .unwrap();
    let config_before = std::fs::read(&config_path).unwrap();
    let creds_before = std::fs::read(&creds_path).unwrap();

    disable_profile(&mut config, &crate::profile::ProfileName::from("acme")).expect("disable");
    assert!(
        config
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .is_disabled()
    );

    let changed = enable_profile(&mut config, &crate::profile::ProfileName::from("acme"))
        .expect("enable succeeds");
    assert!(changed, "a real re-enable must report a change");

    let acme = config
        .find(&crate::profile::ProfileName::from("acme"))
        .unwrap();
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

    let changed = enable_profile(&mut config, &crate::profile::ProfileName::from("acme"))
        .expect("enable on an enabled profile");
    assert!(!changed, "a no-op enable must report no change");
    assert!(
        !config
            .find(&crate::profile::ProfileName::from("acme"))
            .unwrap()
            .is_disabled()
    );
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

    let result = delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("api-acct"),
        false,
        &rotation_guard("api-acct"),
    );
    assert!(
        result.is_err(),
        "a failed settings unwire must fail the whole delete"
    );
    assert!(
        config
            .find(&crate::profile::ProfileName::from("api-acct"))
            .is_some(),
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
    let profile = config
        .find(&crate::profile::ProfileName::from("api-acct"))
        .expect("profile present")
        .clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("api-acct"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &"stale",
    );

    clear_profile_api_key(&mut config, &crate::profile::ProfileName::from("api-acct"))
        .expect("clear api key");

    let profile = config
        .find(&crate::profile::ProfileName::from("api-acct"))
        .expect("profile still present");
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
        &crate::profile::ProfileName::from("api-acct"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    assert!(!cache.exists(), "stale third-party stats cache dropped");

    // The leak fix drops a base_url at the LOAD boundary, so verify the shell
    // survives a reload too: a cleared PURE api account (no OAuth pair) keeps its
    // base_url and is not flipped to an OAuth profile.
    let reloaded = crate::profile::load_profile(&crate::profile::ProfileName::from("api-acct"))
        .expect("reload");
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
    crate::claude::link_profile_credentials(&crate::profile::ProfileName::from("acct"))
        .expect("link acct live");

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from("acct"),
            file,
            &"stale",
        );
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

    clear_profile_credentials(&mut config, &crate::profile::ProfileName::from("acct"))
        .expect("clear credentials");

    let profile = config
        .find(&crate::profile::ProfileName::from("acct"))
        .expect("profile still present");
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

    let cred_path = profile_dir(&crate::profile::ProfileName::from("acct"))
        .unwrap()
        .join("credentials.json");
    assert!(!cred_path.exists(), "credentials.json removed");

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        let path = crate::profile_cache::profile_cache_path(
            &crate::profile::ProfileName::from("acct"),
            file,
        )
        .unwrap();
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
    crate::profile::stage_rotated_credentials(&crate::profile::ProfileName::from("acct"), &creds())
        .expect("stage sidecar");

    let mut other = Profile::new("other".to_string(), None, None);
    other.credentials = Some(creds());
    save_profile(&other).expect("save other");
    crate::claude::link_profile_credentials(&crate::profile::ProfileName::from("other"))
        .expect("link other live");

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

    clear_profile_credentials(&mut config, &crate::profile::ProfileName::from("acct"))
        .expect("clear credentials");

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
    let acct = reloaded
        .find(&crate::profile::ProfileName::from("acct"))
        .expect("acct still present");
    assert!(
        acct.credentials.is_none(),
        "a lingering sidecar must not resurrect the blanked login"
    );
    let cred_path = profile_dir(&crate::profile::ProfileName::from("acct"))
        .unwrap()
        .join("credentials.json");
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
    crate::lock::with_state_lock(|held| {
        finish_switch(
            &mut config,
            &crate::profile::ProfileName::from("acct"),
            held,
        )
    })
    .expect("finish_switch");

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
    crate::claude::link_profile_credentials(&crate::profile::ProfileName::from("acct"))
        .expect("link acct live");

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
    config.set_auth_broken(&crate::profile::ProfileName::from("xfx"), true);
    assert!(
        config.is_auth_broken(&crate::profile::ProfileName::from("xfx")),
        "precondition: quarantined"
    );

    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("xfx"),
        CaptureSnapshot {
            credentials: Some(oauth_creds("fresh-access")),
            base_url: None,
            api_key: None,
            account_uuid: None,
        },
    )
    .expect("re-auth overwrite");

    assert_eq!(
        config
            .find(&crate::profile::ProfileName::from("xfx"))
            .and_then(|p| p.access_token()),
        Some("fresh-access"),
        "credentials overwritten by re-auth",
    );
    assert!(
        !config.is_auth_broken(&crate::profile::ProfileName::from("xfx")),
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

    let err = switch_profile_cli(config, &crate::profile::ProfileName::from("dead-acct"))
        .expect_err("a dead target must be refused");
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
        // The cache write is gated on the on-disk record; pinning an anchor is
        // this helper's whole job, and a skipped write would read as "no anchor".
        crate::testutil::register_names(&[name]);
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(name),
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

// ── a console login never touches the profile's api key ──────────────────────

/// The console callback hands back a WORKSPACE key (`sk-ws-…`) against the
/// workspace endpoint — a different product, billed pay-as-you-go, from the
/// prepaid Token Plan the profile runs on (`sk-sp-…` against
/// `token-plan.<region>.maas.aliyuncs.com`). Persisting it would silently move
/// that account's spend onto the other product, so `store_console_login` writes
/// the session and NOTHING else: not over a stored key, and not into an empty
/// slot either, where it would leave the profile pointed at an endpoint its plan
/// does not cover.
#[test]
fn a_console_login_stores_the_session_and_leaves_the_api_key_alone() {
    let _home = HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let session = || crate::profile::ConsoleCredential {
        token: "3f2b1c4d-5e6f-4708-9a1b-2c3d4e5f6071".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    };

    // A profile already holding its plan key keeps it byte for byte.
    let with_key = Profile::new(
        "qwen-keyed".to_string(),
        Some(base.to_string()),
        Some("sk-sp-the-plan-key".to_string()),
    );
    let mut config = inactive_config(with_key);
    store_console_login(
        &mut config,
        &crate::profile::ProfileName::from("qwen-keyed"),
        session(),
    )
    .expect("store_console_login");
    let loaded = crate::profile::load_profile(&crate::profile::ProfileName::from("qwen-keyed"))
        .expect("load_profile");
    assert_eq!(
        loaded.api_key.as_deref(),
        Some("sk-sp-the-plan-key"),
        "the plan key survives a console login",
    );
    assert_eq!(
        loaded.base_url.as_deref(),
        Some(base),
        "and so does the endpoint"
    );
    assert_eq!(
        loaded.console.expect("the session landed").token,
        "3f2b1c4d-5e6f-4708-9a1b-2c3d4e5f6071"
    );

    // An EMPTY slot is not an invitation: the callback's key belongs to another
    // product, so filling it would point this profile at the wrong endpoint.
    let no_key = Profile::new("qwen-bare".to_string(), Some(base.to_string()), None);
    let mut config = inactive_config(no_key);
    store_console_login(
        &mut config,
        &crate::profile::ProfileName::from("qwen-bare"),
        session(),
    )
    .expect("store_console_login");
    let loaded = crate::profile::load_profile(&crate::profile::ProfileName::from("qwen-bare"))
        .expect("load_profile");
    assert_eq!(loaded.api_key, None, "no key is written into an empty slot");
    assert!(loaded.console.is_some(), "the session still landed");
}

/// `main.rs`'s reauth contract is that the snapshot clears the old type's
/// leftovers. The console session is a FOURTH credential, and it is meaningless
/// off Alibaba: left behind, an endpoint move parks a live Model Studio session
/// on a profile that no longer talks to Model Studio, where it survives every
/// later reload.
#[test]
fn moving_the_endpoint_off_alibaba_clears_the_console_session() {
    let _home = HomeSandbox::new();
    let alibaba = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let session = || crate::profile::ConsoleCredential {
        token: "console-token".to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    };

    // edit_profile_endpoint: Alibaba → z.ai.
    let mut p = Profile::new(
        "moved".to_string(),
        Some(alibaba.to_string()),
        Some("sk-sp-k".to_string()),
    );
    p.console = Some(session());
    let mut config = inactive_config(p);
    edit_profile_endpoint(
        &mut config,
        &crate::profile::ProfileName::from("moved"),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("zai-key".to_string()),
    )
    .expect("edit_profile_endpoint");
    assert!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("moved"))
            .expect("load_profile")
            .console
            .is_none(),
        "the console session does not survive the endpoint moving off Alibaba",
    );

    // …and it DOES survive an edit that stays on Alibaba (a rotated api key).
    let mut p = Profile::new(
        "stayed".to_string(),
        Some(alibaba.to_string()),
        Some("sk-sp-k".to_string()),
    );
    p.console = Some(session());
    let mut config = inactive_config(p);
    edit_profile_endpoint(
        &mut config,
        &crate::profile::ProfileName::from("stayed"),
        Some(alibaba.to_string()),
        Some("sk-sp-rotated".to_string()),
    )
    .expect("edit_profile_endpoint");
    assert!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("stayed"))
            .expect("load_profile")
            .console
            .is_some(),
        "a same-provider edit keeps the session the operator just captured",
    );

    // overwrite_captured_profile: the reauth path, Alibaba → an api-mode login
    // that moves the endpoint off Alibaba. "Preserve key on reauth" (owner,
    // 2026-08-30) keeps the endpoint a browser snapshot omits — and with it the
    // session — so the clearing this leg pins is the SNAPSHOT-side move: the
    // endpoint the reauth replaces off Alibaba takes the session with it.
    let mut p = Profile::new(
        "reauthed".to_string(),
        Some(alibaba.to_string()),
        Some("sk-sp-k".to_string()),
    );
    p.console = Some(session());
    let mut config = inactive_config(p);
    overwrite_captured_profile(
        &mut config,
        &crate::profile::ProfileName::from("reauthed"),
        CaptureSnapshot {
            credentials: None,
            base_url: Some("https://api.z.ai/api/anthropic".to_string()),
            api_key: Some("zai-key".to_string()),
            account_uuid: None,
        },
    )
    .expect("overwrite_captured_profile");
    assert!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("reauthed"))
            .expect("load_profile")
            .console
            .is_none(),
        "a reauth that moves the endpoint off Alibaba clears the session with it",
    );
}
