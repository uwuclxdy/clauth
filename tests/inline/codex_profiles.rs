#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `codex-profiles.toml` load contract: the missing-file default, the on-disk
//! spelling (`wrap_off` included), and tolerance for keys a newer binary may
//! add.

use super::*;
use crate::testutil::HomeSandbox;

fn write_state(body: &str) {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");
    std::fs::write(dir.join("codex-profiles.toml"), body).expect("write codex-profiles.toml");
}

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
    write_state(
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
    write_state("profiles = [\"solo\"]\n");
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
    write_state("profiles = [\"solo\"]\nfrom_the_future = 1\n");
    let state = CodexState::load().expect("load");
    assert_eq!(state.profiles(), ["solo"]);
}

#[test]
fn the_mtime_stat_answers_absent_and_present() {
    let _home = HomeSandbox::new();
    assert_eq!(codex_state_mtime(), None, "no file, no mtime");
    write_state("profiles = []\n");
    assert!(codex_state_mtime().is_some());
}
