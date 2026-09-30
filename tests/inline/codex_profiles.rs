#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `codex-profiles.toml` load contract: the missing-file default, the on-disk
//! spelling (`wrap_off` included), and tolerance for keys a newer binary may
//! add.

use super::*;
use crate::testutil::HomeSandbox;
use crate::testutil::write_codex_state;

#[test]
fn a_missing_file_reads_as_an_empty_roster() {
    let _home = HomeSandbox::new();
    let state = CodexState::load().expect("load");
    assert_eq!(state, CodexState::default());
    assert!(state.profiles().is_empty());
}

/// Pins the on-disk format by reading a hand-written file, exactly the way an
/// operator or an older save would leave it: the four fields, with the
/// wrap-off slot spelled `wrap_off` like its `profiles.toml` twin.
#[test]
fn the_on_disk_spelling_is_the_profiles_toml_one() {
    let _home = HomeSandbox::new();
    write_codex_state(
        "active_profile = \"work\"\nprofiles = [\"work\", \"play\"]\nfallback_chain = [\"work\"]\nwrap_off = true\n",
    );
    let state = CodexState::load().expect("load");
    assert_eq!(state.active_profile.as_deref(), Some("work"));
    assert_eq!(state.profiles(), ["work", "play"]);
    assert_eq!(state.fallback_chain, ["work"]);
    assert!(state.switch_off_when_spent);
}

/// Every field but the roster may be absent — a file a first codex profile
/// creation writes carries only what is set.
#[test]
fn a_minimal_file_defaults_the_rest() {
    let _home = HomeSandbox::new();
    write_codex_state("profiles = [\"solo\"]\n");
    let state = CodexState::load().expect("load");
    assert_eq!(state.profiles(), ["solo"]);
    assert_eq!(state.active_profile, None);
    assert!(state.fallback_chain.is_empty());
    assert!(!state.switch_off_when_spent);
}

/// A key this binary does not know must not fail the load: the file is owned
/// by whichever clauth wrote it last, and a newer one may know more fields.
#[test]
fn an_unknown_key_does_not_fail_the_load() {
    let _home = HomeSandbox::new();
    write_codex_state("profiles = [\"solo\"]\nfrom_the_future = 1\n");
    let state = CodexState::load().expect("load");
    assert_eq!(state.profiles(), ["solo"]);
}

#[test]
fn the_mtime_stat_answers_absent_and_present() {
    let _home = HomeSandbox::new();
    assert_eq!(codex_state_mtime(), None, "no file, no mtime");
    write_codex_state("profiles = []\n");
    assert!(codex_state_mtime().is_some());
}

/// The codex chain's weekly line is the codex file's own, under the claude key
/// (`weekly_switch_threshold`): absent reads as the shared default, a value
/// inside the band is honored, and an out-of-band hand-edit resets to the
/// default rather than clamping — the same fail-safe the claude accessor has.
#[test]
fn the_weekly_line_is_the_codex_files_own_with_the_claude_default() {
    let _home = HomeSandbox::new();
    write_codex_state("profiles = [\"solo\"]\n");
    assert_eq!(
        CodexState::load()
            .expect("load")
            .weekly_switch_threshold_pct(),
        DEFAULT_WEEKLY_SWITCH_PCT
    );
    assert_eq!(DEFAULT_WEEKLY_SWITCH_PCT, 98.0);

    write_codex_state("profiles = [\"solo\"]\nweekly_switch_threshold = 50.0\n");
    assert_eq!(
        CodexState::load()
            .expect("load")
            .weekly_switch_threshold_pct(),
        50.0
    );

    for garbage in ["0.98", "40", "120.0", "nan"] {
        write_codex_state(&format!(
            "profiles = [\"solo\"]\nweekly_switch_threshold = {garbage}\n"
        ));
        assert_eq!(
            CodexState::load()
                .expect("load")
                .weekly_switch_threshold_pct(),
            DEFAULT_WEEKLY_SWITCH_PCT,
            "out-of-band {garbage} resets to the default"
        );
    }
}

/// A save never invents the key: a file without it stays without it across a
/// real mutation, and one carrying it keeps the value — so a hand-edited
/// default-less file is not rewritten into a 98.0 the operator never typed.
#[test]
fn a_save_keeps_the_weekly_key_exactly_as_it_found_it() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");

    write_codex_state("profiles = [\"a\", \"b\"]\n");
    CodexState::update(|state| {
        state.set_active(Some("b"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "active_profile = \"b\"\nprofiles = [\n    \"a\",\n    \"b\",\n]\nfallback_chain = []\nwrap_off = false\n"
    );

    write_codex_state("profiles = [\"a\", \"b\"]\nweekly_switch_threshold = 75.0\n");
    CodexState::update(|state| {
        state.set_active(Some("a"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "active_profile = \"a\"\nprofiles = [\n    \"a\",\n    \"b\",\n]\nfallback_chain = []\nwrap_off = false\nweekly_switch_threshold = 75.0\n"
    );
}

/// An out-of-band hand-edit is normalized at load, the way `load_app_state`
/// normalizes the claude line: the field itself reads as the default, so the
/// next real save heals the file instead of carrying a line the walk never
/// used, while a no-op save still leaves the bytes alone.
#[test]
fn an_out_of_band_weekly_line_heals_on_the_next_save() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    let raw = "profiles = [\"a\", \"b\"]\nweekly_switch_threshold = 120.0\n";
    write_codex_state(raw);

    let loaded = CodexState::load().expect("load");
    assert_eq!(
        loaded.weekly_switch_threshold,
        Some(DEFAULT_WEEKLY_SWITCH_PCT)
    );
    assert_eq!(
        loaded.weekly_switch_threshold_pct(),
        DEFAULT_WEEKLY_SWITCH_PCT
    );

    CodexState::update(|_state| Ok(())).expect("no-op update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        raw,
        "a no-op save leaves a hand-edited file alone"
    );

    CodexState::update(|state| {
        state.set_active(Some("b"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "active_profile = \"b\"\nprofiles = [\n    \"a\",\n    \"b\",\n]\nfallback_chain = []\nwrap_off = false\nweekly_switch_threshold = 98.0\n"
    );
}
