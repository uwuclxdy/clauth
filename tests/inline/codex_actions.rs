//! Codex capture/switch semantics (CDX-1 T3/T4) under a sandboxed HOME.
//! Fixture tokens are fakes; the real `~/.codex` is never touched.

use super::*;
use crate::profile::Harness;
use crate::testutil::HomeSandbox;

fn auth_bytes(access: &str, account_id: &str) -> Vec<u8> {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": access,
            "refresh_token": format!("rt-{access}"),
            "account_id": account_id,
        },
        "agent_identity": { "unmodeled": true },
    })
    .to_string()
    .into_bytes()
}

fn empty_config() -> AppConfig {
    AppConfig {
        state: Default::default(),
        profiles: Vec::new(),
    }
}

fn live() -> Vec<u8> {
    crate::codex::read_live().unwrap().expect("live auth.json")
}

#[test]
fn capture_creates_an_active_codex_profile() {
    let sandbox = HomeSandbox::new();
    let alpha = auth_bytes("at-alpha", "acct-alpha");
    crate::codex::write_live(&alpha).unwrap();

    let mut cfg = empty_config();
    codex_capture_into_profile(&mut cfg, "cdx-a").expect("capture");

    let profile = cfg.find("cdx-a").expect("profile exists");
    assert_eq!(profile.harness, Harness::Codex);
    assert_eq!(
        crate::codex::read_profile_auth("cdx-a").unwrap().as_deref(),
        Some(&alpha[..]),
        "stored bytes are the live bytes, verbatim"
    );
    assert_eq!(cfg.state.active_codex_profile.as_deref(), Some("cdx-a"));
    assert!(
        cfg.state.active_profile.is_none(),
        "the claude slot is never touched"
    );
    let config_toml =
        std::fs::read_to_string(sandbox.home().join(".clauth/profiles/cdx-a/config.toml")).unwrap();
    assert!(config_toml.contains("harness = \"codex\""));
}

#[test]
fn capture_refuses_without_a_usable_live_login() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let err = codex_capture_into_profile(&mut cfg, "cdx-a").unwrap_err();
    assert!(err.to_string().contains("codex login"), "{err}");

    crate::codex::write_live(br#"{"tokens":{"access_token":"","refresh_token":""}}"#).unwrap();
    let err = codex_capture_into_profile(&mut cfg, "cdx-a").unwrap_err();
    assert!(err.to_string().contains("logged-out shell"), "{err}");
}

#[test]
fn capture_refuses_under_non_file_store_mode() {
    let sandbox = HomeSandbox::new();
    let dir = sandbox.home().join(".codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "cli_auth_credentials_store = \"keyring\"\n",
    )
    .unwrap();
    crate::codex::write_live(&auth_bytes("at-alpha", "acct-alpha")).unwrap();

    let mut cfg = empty_config();
    let err = codex_capture_into_profile(&mut cfg, "cdx-a").unwrap_err();
    assert!(err.to_string().contains("keyring"), "{err}");
}

// CAP-3 sibling: one account, one profile. Re-auth of the holder is a
// refresh, not a duplicate.
#[test]
fn capture_dedups_the_account_across_codex_profiles() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    crate::codex::write_live(&auth_bytes("at-alpha", "acct-alpha")).unwrap();
    codex_capture_into_profile(&mut cfg, "cdx-a").expect("first capture");

    let err = codex_capture_into_profile(&mut cfg, "cdx-b").unwrap_err();
    assert!(err.to_string().contains("cdx-a"), "{err}");

    // Same account, rotated token → re-auth in place succeeds.
    let rotated = auth_bytes("at-alpha-ROTATED", "acct-alpha");
    crate::codex::write_live(&rotated).unwrap();
    codex_capture_into_profile(&mut cfg, "cdx-a").expect("re-auth");
    assert_eq!(
        crate::codex::read_profile_auth("cdx-a").unwrap().as_deref(),
        Some(&rotated[..])
    );
}

#[test]
fn capture_refuses_a_claude_profile_name() {
    let _sandbox = HomeSandbox::new();
    crate::codex::write_live(&auth_bytes("at-alpha", "acct-alpha")).unwrap();
    let mut cfg = empty_config();
    cfg.add(crate::testutil::blank_profile("work"));
    let err = codex_capture_into_profile(&mut cfg, "work").unwrap_err();
    assert!(err.to_string().contains("claude profile"), "{err}");
}

/// Two captured codex accounts, live currently holds `cdx-b`'s chain.
fn two_account_setup(cfg: &mut AppConfig) -> (Vec<u8>, Vec<u8>) {
    let alpha = auth_bytes("at-alpha", "acct-alpha");
    let beta = auth_bytes("at-beta", "acct-beta");
    crate::codex::write_live(&alpha).unwrap();
    codex_capture_into_profile(cfg, "cdx-a").expect("capture alpha");
    crate::codex::write_live(&beta).unwrap();
    codex_capture_into_profile(cfg, "cdx-b").expect("capture beta");
    (alpha, beta)
}

#[test]
fn switch_installs_the_target_chain() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let (alpha, beta) = two_account_setup(&mut cfg);

    let report =
        codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Refuse).expect("switch");
    assert_eq!(live(), alpha, "live now holds the target chain");
    assert_eq!(cfg.state.active_codex_profile.as_deref(), Some("cdx-a"));
    assert!(cfg.state.active_profile.is_none(), "claude slot untouched");
    assert!(report.adopted_back.is_none() && report.archived.is_none());
    assert_eq!(
        crate::codex::read_profile_auth("cdx-b").unwrap().as_deref(),
        Some(&beta[..]),
        "outgoing store unchanged when live matched it byte-for-byte"
    );
}

#[test]
fn switch_adopts_back_a_rotated_outgoing_chain() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let (alpha, _beta) = two_account_setup(&mut cfg);

    // codex refreshed cdx-b's chain since our snapshot: same account, new token.
    let rotated = auth_bytes("at-beta-ROTATED", "acct-beta");
    crate::codex::write_live(&rotated).unwrap();

    let report =
        codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Refuse).expect("switch");
    assert_eq!(report.adopted_back.as_deref(), Some("cdx-b"));
    assert_eq!(
        crate::codex::read_profile_auth("cdx-b").unwrap().as_deref(),
        Some(&rotated[..]),
        "the rotation was adopted back before the install — loss-free"
    );
    assert_eq!(live(), alpha);
}

// Switching TO the profile that already owns the live login must never roll
// the live chain back to a stale snapshot — the live file is the truth.
#[test]
fn switch_to_the_live_owner_never_rolls_the_chain_back() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    two_account_setup(&mut cfg);

    let rotated = auth_bytes("at-beta-ROTATED", "acct-beta");
    crate::codex::write_live(&rotated).unwrap();

    let report =
        codex_switch_profile(&mut cfg, "cdx-b", ForeignLivePolicy::Refuse).expect("switch");
    assert_eq!(report.adopted_back.as_deref(), Some("cdx-b"));
    assert_eq!(live(), rotated, "live keeps the fresher chain");
    assert_eq!(
        crate::codex::read_profile_auth("cdx-b").unwrap().as_deref(),
        Some(&rotated[..])
    );
    assert_eq!(cfg.state.active_codex_profile.as_deref(), Some("cdx-b"));
}

#[test]
fn switch_over_a_foreign_login_refuses_or_archives_by_policy() {
    let sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let (alpha, _beta) = two_account_setup(&mut cfg);

    let foreign = auth_bytes("at-FOREIGN", "acct-foreign");
    crate::codex::write_live(&foreign).unwrap();

    let err = codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Refuse).unwrap_err();
    assert!(
        err.to_string().contains("matches no stored profile"),
        "{err}"
    );
    assert_eq!(live(), foreign, "refuse leaves the live login alone");

    let report =
        codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Archive).expect("switch");
    let archived = report.archived.expect("archived path");
    let saved = std::fs::read_to_string(&archived).unwrap();
    assert!(saved.contains("at-FOREIGN"), "quarantine holds the login");
    assert!(
        archived.starts_with(sandbox.home().join(".clauth/quarantine")),
        "archived under quarantine"
    );
    assert_eq!(live(), alpha);
}

#[test]
fn switch_archives_unparseable_live_bytes() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let (alpha, _beta) = two_account_setup(&mut cfg);

    crate::codex::write_live(b"corrupt \x00 not-json").unwrap();
    let report =
        codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Refuse).expect("switch");
    assert!(report.archived.is_some(), "unparseable bytes are preserved");
    assert_eq!(live(), alpha);
}

// The harness guards hold in both directions at the action layer, so no
// caller dispatch mistake can cross the streams.
#[test]
fn cross_harness_switches_are_refused() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    crate::codex::write_live(&auth_bytes("at-alpha", "acct-alpha")).unwrap();
    codex_capture_into_profile(&mut cfg, "cdx-a").unwrap();
    cfg.add(crate::testutil::blank_profile("work"));

    let err = switch_profile(&mut cfg, "cdx-a").unwrap_err();
    assert!(err.to_string().contains("codex profile"), "{err}");

    let err = codex_switch_profile(&mut cfg, "work", ForeignLivePolicy::Refuse).unwrap_err();
    assert!(err.to_string().contains("claude profile"), "{err}");
}

#[test]
fn switch_refuses_a_quarantined_or_empty_target() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    two_account_setup(&mut cfg);

    cfg.set_auth_broken("cdx-a", true);
    let err = codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Refuse).unwrap_err();
    assert!(err.to_string().contains("quarantined"), "{err}");
    cfg.set_auth_broken("cdx-a", false);

    // A codex profile with no stored login yet is not a switch target.
    let mut ghost = crate::testutil::blank_profile("cdx-ghost");
    ghost.harness = Harness::Codex;
    cfg.add(ghost);
    let err = codex_switch_profile(&mut cfg, "cdx-ghost", ForeignLivePolicy::Refuse).unwrap_err();
    assert!(err.to_string().contains("capture one first"), "{err}");
}

// Codex logout drops the stored snapshot + active marker; the profile shell
// and the LIVE login both survive (the live file is codex's own).
#[test]
fn clear_codex_auth_drops_store_and_marker_but_never_live() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let (_alpha, beta) = two_account_setup(&mut cfg);

    codex_clear_profile_auth(&mut cfg, "cdx-b").expect("logout");
    assert!(crate::codex::read_profile_auth("cdx-b").unwrap().is_none());
    assert!(cfg.state.active_codex_profile.is_none());
    assert!(cfg.find("cdx-b").is_some(), "the profile shell survives");
    assert_eq!(
        crate::codex::read_live().unwrap().as_deref(),
        Some(&beta[..]),
        "the live login is codex's own — never touched by a logout"
    );

    let err = codex_clear_profile_auth(&mut cfg, "work-missing").unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

// The two quiet live states at switch time: a MISSING live file (fresh
// machine / codex logout removed it) and a logged-out SHELL both mean nothing
// to preserve — the switch proceeds with no adopt-back and no archive.
#[test]
fn switch_proceeds_over_a_missing_or_shell_live_login() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    let (alpha, beta) = two_account_setup(&mut cfg);

    std::fs::remove_file(crate::codex::live_auth_path().unwrap()).unwrap();
    let report =
        codex_switch_profile(&mut cfg, "cdx-a", ForeignLivePolicy::Refuse).expect("switch");
    assert_eq!(live(), alpha, "missing live → target installed directly");
    assert!(report.adopted_back.is_none() && report.archived.is_none());

    crate::codex::write_live(br#"{"tokens":{"access_token":"","refresh_token":""}}"#).unwrap();
    let report =
        codex_switch_profile(&mut cfg, "cdx-b", ForeignLivePolicy::Refuse).expect("switch");
    assert_eq!(
        live(),
        beta,
        "a logged-out shell is overwritten without loss"
    );
    assert!(
        report.archived.is_none(),
        "a shell holds nothing worth archiving"
    );
}

// CDX-1 review fix: claude-shaped credentials must never land in a codex
// profile through the reauth writer (harness immutability, reverse direction).
#[test]
fn overwrite_captured_profile_refuses_a_codex_target() {
    let _sandbox = HomeSandbox::new();
    let mut cfg = empty_config();
    crate::codex::write_live(&auth_bytes("at-alpha", "acct-alpha")).unwrap();
    codex_capture_into_profile(&mut cfg, "cdx-a").unwrap();

    let snapshot = CaptureSnapshot {
        credentials: None,
        base_url: Some("https://api.anthropic.com".into()),
        api_key: Some("sk-claude".into()),
        identity: CaptureIdentity::Unknown,
    };
    let err = overwrite_captured_profile(&mut cfg, "cdx-a", snapshot).unwrap_err();
    assert!(err.to_string().contains("--codex"), "{err}");
    assert!(
        cfg.find("cdx-a").unwrap().api_key.is_none(),
        "nothing claude-shaped landed in the codex profile"
    );
}

// ---------------------------------------------------------------------------
// CDX-3: standby-candidate exclusivity + install-time guards
// ---------------------------------------------------------------------------

fn seeded_codex_config(names: &[&str]) -> AppConfig {
    let mut cfg = empty_config();
    for name in names {
        let mut p = crate::testutil::blank_profile(name);
        p.harness = Harness::Codex;
        cfg.add(p);
        crate::codex::write_profile_auth(
            name,
            &auth_bytes(&format!("at-{name}"), &format!("acct-{name}")),
        )
        .unwrap();
    }
    cfg
}

/// Hold a live codex session lease for `name` (the flock a real
/// `clauth start` session would hold). Keep the returned file alive for the
/// lease's duration.
fn hold_codex_lease(name: &str) -> std::fs::File {
    let dir = crate::runtime::codex_sessions_dir(name).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let file =
        crate::runtime::open_pid_file(&dir.join(format!("{}-t", std::process::id()))).unwrap();
    file.lock().unwrap();
    file
}

#[test]
fn standby_candidates_exclude_every_non_exclusive_chain() {
    let _home = HomeSandbox::new();
    let mut cfg = seeded_codex_config(&["parked", "liveowner", "broken", "leased", "tokenless"]);
    // liveowner: its account holds the live file — codex carries that chain.
    crate::codex::write_live(&auth_bytes("at-liveowner", "acct-liveowner")).unwrap();
    // broken: quarantined after a permanent refresh rejection.
    cfg.set_auth_broken("broken", true);
    // tokenless: stored file with no refresh token — nothing to spend.
    crate::codex::write_profile_auth(
        "tokenless",
        serde_json::json!({ "tokens": { "access_token": "at-only", "account_id": "acct-t" } })
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    // leased: a live isolated session carries its chain.
    let _lease = hold_codex_lease("leased");

    let names: Vec<String> = codex_standby_candidates(&cfg)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(
        names,
        vec!["parked".to_string()],
        "only the exclusively-held chain qualifies"
    );
}

#[test]
fn switch_and_capture_refuse_while_a_refresh_holds_the_rotation_lock() {
    let _home = HomeSandbox::new();
    let mut cfg = seeded_codex_config(&["target"]);
    crate::codex::write_live(&auth_bytes("at-live", "acct-live")).unwrap();
    // A standby refresh in another thread/process holds the rotation lock.
    let _guard = crate::runtime::RotationGuard::acquire("target").unwrap();

    let err = codex_switch_profile(&mut cfg, "target", ForeignLivePolicy::Archive)
        .expect_err("switch must refuse");
    assert!(err.to_string().contains("in flight"), "{err}");

    let err = codex_capture_into_profile(&mut cfg, "target").expect_err("capture must refuse");
    assert!(err.to_string().contains("in flight"), "{err}");
}

#[test]
fn switch_and_capture_refuse_a_leased_profile() {
    let _home = HomeSandbox::new();
    let mut cfg = seeded_codex_config(&["leased"]);
    crate::codex::write_live(&auth_bytes("at-live", "acct-live")).unwrap();
    let _lease = hold_codex_lease("leased");

    let err = codex_switch_profile(&mut cfg, "leased", ForeignLivePolicy::Archive)
        .expect_err("switch must refuse a leased target");
    assert!(err.to_string().contains("clauth start"), "{err}");

    let err = codex_capture_into_profile(&mut cfg, "leased").expect_err("capture must refuse");
    assert!(err.to_string().contains("clauth start"), "{err}");
}

// ---------------------------------------------------------------------------
// CDX-3 R5: the browser-PKCE store path
// ---------------------------------------------------------------------------

#[test]
fn browser_login_store_never_touches_live_or_the_active_slot() {
    let _home = HomeSandbox::new();
    let live_bytes = auth_bytes("at-live", "acct-live");
    crate::codex::write_live(&live_bytes).unwrap();
    let mut cfg = empty_config();

    let minted = auth_bytes("at-minted", "acct-minted");
    codex_store_browser_login(&mut cfg, "cdx-new", &minted).expect("store");

    assert_eq!(
        crate::codex::read_profile_auth("cdx-new")
            .unwrap()
            .as_deref(),
        Some(&minted[..])
    );
    assert_eq!(live(), live_bytes, "live auth.json untouched");
    assert!(
        cfg.state.active_codex_profile.is_none(),
        "browser login never flips the active slot"
    );
    assert_eq!(cfg.find("cdx-new").map(|p| p.harness), Some(Harness::Codex));
}

#[test]
fn browser_login_store_dedups_accounts_and_heals_quarantine() {
    let _home = HomeSandbox::new();
    let mut cfg = seeded_codex_config(&["existing"]);

    // Same account under a new name → CAP-3 refusal names the owner.
    let dup = auth_bytes("at-fresh", "acct-existing");
    let err = codex_store_browser_login(&mut cfg, "second", &dup).expect_err("dup refused");
    assert!(err.to_string().contains("existing"), "{err}");

    // Re-auth of a quarantined profile heals the flag.
    cfg.set_auth_broken("existing", true);
    let fresh = auth_bytes("at-fresh2", "acct-existing");
    codex_store_browser_login(&mut cfg, "existing", &fresh).expect("re-auth in place");
    assert!(
        !cfg.is_auth_broken("existing"),
        "fresh login clears quarantine"
    );
}

#[test]
fn browser_login_store_rejects_a_claude_name_and_shell_bytes() {
    let _home = HomeSandbox::new();
    let mut cfg = empty_config();
    cfg.add(crate::testutil::blank_profile("claude-p"));

    let minted = auth_bytes("at-m", "acct-m");
    let err = codex_store_browser_login(&mut cfg, "claude-p", &minted).expect_err("cross-harness");
    assert!(err.to_string().contains("claude profile"), "{err}");

    let shell = serde_json::json!({ "tokens": {} }).to_string().into_bytes();
    let err = codex_store_browser_login(&mut cfg, "fresh", &shell).expect_err("no tokens");
    assert!(err.to_string().contains("no tokens"), "{err}");
}
