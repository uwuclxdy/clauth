//! `clauth doctor` pure-core tests — the check-formatting + classification logic
//! (freshness, version/schema skew, exit-code aggregation, the RFC3339 parse, and
//! line rendering). The impure probes (launchctl / codesign / security / socket)
//! are operator-run and not exercised here — no real Keychain is ever touched.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use super::*;

#[test]
fn freshness_tracks_the_1s_write_cadence_not_the_refresh_interval() {
    // status.json is rewritten every 1s tick; a healthy file is seconds-fresh
    // regardless of the (unrelated) usage refresh interval. Bands are anchored to
    // that cadence, bounded by the ~60s watchdog.
    assert_eq!(freshness(Duration::from_secs(3)), Status::Pass);
    assert_eq!(freshness(Duration::from_secs(10)), Status::Pass); // ==10s boundary
    assert_eq!(freshness(Duration::from_secs(40)), Status::Warn); // >10s, <=75s
    assert_eq!(freshness(Duration::from_secs(75)), Status::Warn); // ==75s boundary
    assert_eq!(freshness(Duration::from_secs(120)), Status::Fail); // past the watchdog → dead
}

#[test]
fn exit_code_is_nonzero_only_when_something_failed() {
    assert_eq!(
        exit_code(&[Check::pass("a", "ok"), Check::pass("b", "ok")]),
        0
    );
    // A WARN alone never fails the run.
    assert_eq!(exit_code(&[Check::warn("a", "meh", "do x")]), 0);
    assert_eq!(
        exit_code(&[Check::pass("a", "ok"), Check::fail("b", "broken", "fix it")]),
        1
    );
}

#[test]
fn skew_classifies_version_and_schema_mismatches() {
    // Exact match → Pass.
    let (s, _) = skew("0.7.1", 1, Some("0.7.1"), Some(1));
    assert_eq!(s, Status::Pass);
    // Same schema, different version → Warn (a restart adopts the new binary).
    let (s, d) = skew("0.7.1", 1, Some("0.7.0"), Some(1));
    assert_eq!(s, Status::Warn);
    assert!(d.contains("0.7.0") && d.contains("0.7.1"));
    // Schema mismatch → Fail (the read format diverged).
    let (s, _) = skew("0.7.1", 1, Some("0.7.1"), Some(2));
    assert_eq!(s, Status::Fail);
    // No fields in status.json → Warn (old/absent daemon).
    let (s, _) = skew("0.7.1", 1, None, None);
    assert_eq!(s, Status::Warn);
}

#[test]
fn iso_to_ms_round_trips_the_daemon_writer_format() {
    // Couple the reader to the ACTUAL writer (`+00:00`, not `Z`) so a writer
    // format change can't silently break this parse and blank the freshness check.
    use crate::usage::epoch_secs_to_iso;
    assert_eq!(iso_to_ms(&epoch_secs_to_iso(0)), Some(0));
    assert_eq!(
        iso_to_ms(&epoch_secs_to_iso(1_609_459_200)),
        Some(1_609_459_200_000)
    );
    // Also tolerant of the `Z` / fractional shapes (only chars 0..19 are read).
    assert_eq!(iso_to_ms("2021-01-01T00:00:00Z"), Some(1_609_459_200_000));
    assert_eq!(
        iso_to_ms("2021-01-01T00:00:00.123Z"),
        Some(1_609_459_200_000)
    );
    // Too short → None (caller falls back to file mtime).
    assert_eq!(iso_to_ms("2021-01-01"), None);
}

#[test]
fn render_shows_a_fix_only_when_not_passing() {
    let pass = Check::pass("socket", "responds").render();
    assert!(pass.starts_with("[PASS] socket — responds"));
    assert!(!pass.contains("fix:"));

    let fail = Check::fail("socket", "no reply", "restart the daemon").render();
    assert!(fail.starts_with("[FAIL] socket — no reply"));
    assert!(fail.contains("fix: restart the daemon"));
}

// ---- CDX-1 T9: check_codex (sandboxed — no real ~/.codex, WARN-only) ----

mod codex_check {
    use crate::doctor::check_codex;
    use crate::doctor::core::Status;
    use crate::profile::{AppState, save_app_state, save_profile};
    use crate::testutil::{HomeSandbox, blank_profile, set_mtime};

    fn auth_bytes(access: &str, account_id: &str) -> Vec<u8> {
        serde_json::json!({
            "tokens": {
                "access_token": access,
                "refresh_token": format!("rt-{access}"),
                "account_id": account_id,
            },
        })
        .to_string()
        .into_bytes()
    }

    /// Persist one codex profile (+ optional active marker) into the sandbox.
    fn seed_codex_profile(name: &str, active: bool) {
        let mut p = blank_profile(name);
        p.harness = crate::profile::Harness::Codex;
        save_profile(&p).expect("persist profile");
        crate::codex::write_profile_auth(name, &auth_bytes("at-a", "acct-a")).unwrap();
        let state = AppState {
            profiles: vec![name.into()],
            active_codex_profile: active.then(|| name.into()),
            ..AppState::default()
        };
        save_app_state(&state).expect("persist state");
    }

    // CDX-3 R6: a quarantined codex profile outranks every live-state line —
    // the chain is dead and only a fresh login fixes it.
    #[test]
    fn warns_on_a_quarantined_codex_profile() {
        let _home = HomeSandbox::new();
        let mut p = blank_profile("cdx-dead");
        p.harness = crate::profile::Harness::Codex;
        save_profile(&p).unwrap();
        crate::codex::write_profile_auth("cdx-dead", &auth_bytes("at-d", "acct-d")).unwrap();
        save_app_state(&AppState {
            profiles: vec!["cdx-dead".into()],
            auth_broken: vec!["cdx-dead".into()],
            ..AppState::default()
        })
        .unwrap();
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("quarantined"), "{}", check.detail);
        assert!(check.detail.contains("cdx-dead"), "{}", check.detail);
    }

    // CDX-3 R6: a stored chain whose last_refresh is far past the keep-alive
    // line means the standby refresh isn't landing — surface it.
    #[test]
    fn warns_when_standby_keep_alive_is_not_landing() {
        let _home = HomeSandbox::new();
        let mut p = blank_profile("cdx-stale");
        p.harness = crate::profile::Harness::Codex;
        save_profile(&p).unwrap();
        let old = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() - 20 * 86_400);
        let bytes = serde_json::json!({
            "tokens": {
                "access_token": "at-s",
                "refresh_token": "rt-s",
                "account_id": "acct-s",
            },
            "last_refresh": old,
        })
        .to_string()
        .into_bytes();
        crate::codex::write_profile_auth("cdx-stale", &bytes).unwrap();
        save_app_state(&AppState {
            profiles: vec!["cdx-stale".into()],
            ..AppState::default()
        })
        .unwrap();
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("last refreshed"), "{}", check.detail);
    }

    // CDX-5: the proxy check is silent until a heartbeat exists, WARNs when
    // stale, and PASSes when fresh (+notes the config-pointed state).
    #[test]
    fn codex_proxy_check_tracks_the_heartbeat() {
        use crate::doctor::check_codex_proxy;
        let _home = HomeSandbox::new();
        assert!(check_codex_proxy().is_none(), "no heartbeat → no line");

        // Fresh heartbeat, config NOT pointed → PASS with the nudge.
        crate::proxy::touch_heartbeat_for_test(4517);
        let check = check_codex_proxy().expect("line");
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("NOT pointed"), "{}", check.detail);

        // Point the config → PASS clean.
        let codex_dir = crate::codex::codex_dir().unwrap();
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "model_provider = \"clauth\"\n",
        )
        .unwrap();
        let check = check_codex_proxy().expect("line");
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("points at it"), "{}", check.detail);
    }

    // A claude-only install gets NO codex line at all.
    #[test]
    fn silent_when_no_codex_profile_exists() {
        let _home = HomeSandbox::new();
        save_profile(&blank_profile("work")).unwrap();
        save_app_state(&AppState {
            profiles: vec!["work".into()],
            ..AppState::default()
        })
        .unwrap();
        assert!(check_codex().is_none());
    }

    #[test]
    fn passes_when_live_matches_the_active_profile() {
        let _home = HomeSandbox::new();
        seed_codex_profile("cdx-a", true);
        crate::codex::write_live(&auth_bytes("at-a-rotated", "acct-a")).unwrap();
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Pass, "{}", check.render());
    }

    #[test]
    fn warns_without_a_live_login() {
        let _home = HomeSandbox::new();
        seed_codex_profile("cdx-a", true);
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn, "{}", check.render());
    }

    #[test]
    fn warns_on_non_file_store_mode() {
        let home = HomeSandbox::new();
        seed_codex_profile("cdx-a", true);
        let dir = home.home().join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn, "{}", check.render());
        assert!(check.render().contains("keyring"));
    }

    #[test]
    fn warns_when_live_is_a_different_account() {
        let _home = HomeSandbox::new();
        seed_codex_profile("cdx-a", true);
        crate::codex::write_live(&auth_bytes("at-x", "acct-OTHER")).unwrap();
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn, "{}", check.render());
    }

    // Refresh-token server TTL is unknown — a week-old parked snapshot gets a
    // staleness warning (PLAN.md §0.8 mitigation).
    #[test]
    fn warns_on_a_week_old_snapshot() {
        let _home = HomeSandbox::new();
        seed_codex_profile("cdx-a", true);
        crate::codex::write_live(&auth_bytes("at-a", "acct-a")).unwrap();
        let path = crate::codex::profile_auth_path("cdx-a").unwrap();
        set_mtime(
            &path,
            std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400),
        );
        let check = check_codex().expect("codex line");
        assert_eq!(check.status, Status::Warn, "{}", check.render());
        assert!(check.render().contains("days old"), "{}", check.render());
    }
}
