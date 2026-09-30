//! Storage tests for the per-account note file.

use super::*;
use crate::profile::Profile;
use crate::testutil::HomeSandbox;
use std::collections::BTreeMap;

/// A roster entry on disk, so `is_configured` passes the save gate. Mirrors
/// the tui_app tests' `mini_profile` (the house's per-file test builders).
fn seed_roster(name: &str) {
    crate::profile::save_profile(&mini_profile(name)).unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec![name.into()],
        ..Default::default()
    })
    .unwrap();
}

fn mini_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        preferred_days: Vec::new(),
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
    }
}

/// A note round-trips through the profile dir as plain text.
#[test]
fn a_note_round_trips_through_the_profile_dir() {
    let _home = HomeSandbox::new();
    let name = ProfileName::from("alice");
    seed_roster("alice");
    super::save_note(&name, "hello\nworld").unwrap();
    assert_eq!(
        super::load_note(&name).as_deref(),
        Some("hello\nworld"),
        "the stored note reads back verbatim"
    );
}

/// The note file is 0600 in a 0700 dir — the tree-wide writer rule.
#[cfg(unix)]
#[test]
fn the_note_file_and_its_dir_are_owner_only() {
    let _home = HomeSandbox::new();
    let name = ProfileName::from("alice");
    seed_roster("alice");
    super::save_note(&name, "secretish").unwrap();
    let path = crate::profile_cache::profile_cache_path(&name, super::NOTE_FILE).unwrap();
    let mode = |p: &std::path::Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode(&path), 0o600, "the note file is 0600");
    assert_eq!(
        mode(path.parent().unwrap()),
        0o700,
        "the profile dir is 0700"
    );
}

/// Saving an empty note removes the file; the tab returns to its hint.
#[test]
fn an_empty_note_removes_the_file() {
    let _home = HomeSandbox::new();
    let name = ProfileName::from("alice");
    seed_roster("alice");
    super::save_note(&name, "text").unwrap();
    super::save_note(&name, "").unwrap();
    assert_eq!(super::load_note(&name), None, "the empty note cleared it");
}

/// A save for a name the roster dropped is refused — it must not re-create a
/// deleted account's directory.
#[test]
fn a_note_for_a_deleted_account_is_refused() {
    let _home = HomeSandbox::new();
    let name = ProfileName::from("ghost");
    let err = super::save_note(&name, "boo").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(
        !crate::profile_cache::profile_cache_path(&name, super::NOTE_FILE)
            .unwrap()
            .exists(),
        "no directory was re-created"
    );
}
