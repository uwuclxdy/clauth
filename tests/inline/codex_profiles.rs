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
        "profiles = [\"a\", \"b\"]\nactive_profile = \"b\"\n"
    );

    write_codex_state("profiles = [\"a\", \"b\"]\nweekly_switch_threshold = 75.0\n");
    CodexState::update(|state| {
        state.set_active(Some("a"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "profiles = [\"a\", \"b\"]\nweekly_switch_threshold = 75.0\nactive_profile = \"a\"\n"
    );
}

/// The switch-off marker's on-disk contract: a key a newer clauth wrote is
/// read back, and the first switch drops the key from the file again — a
/// save with the marker cleared never invents it, the same tolerance an
/// older binary shows any unknown key.
#[test]
fn the_switch_off_marker_reads_back_and_a_switch_drops_the_key() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");

    write_codex_state("profiles = [\"a\", \"b\"]\nswitched_off = true\n");
    assert!(
        CodexState::load().expect("load").switched_off(),
        "the marker loads from disk"
    );

    CodexState::update(|state| {
        state.set_active(Some("a"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "profiles = [\"a\", \"b\"]\nactive_profile = \"a\"\n",
        "the switch clears the marker and the save drops the key"
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
        "profiles = [\"a\", \"b\"]\nweekly_switch_threshold = 98.0\nactive_profile = \"b\"\n"
    );
}

#[test]
fn a_codex_update_edits_in_place_keeping_unknown_keys() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    let fixture = "# top note\nfrom_the_future = 1\nwrap_off = true # mine\n\
                   profiles = [\n  # work\n  \"a\",\n  \"b\",\n]\n";
    write_codex_state(fixture);
    CodexState::update(|state| {
        state.set_active(Some("b"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        format!("{fixture}active_profile = \"b\"\n"),
    );
}

#[test]
fn removing_a_codex_profile_keeps_the_comments_around_it() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state(
        "profiles = [\n  # one\n  \"a\",\n  # two\n  \"b\",\n]\n\
         fallback_chain = [\"a\", \"b\"]\n\n[ids]\n# ids note\na = \"id-a\"\nb = \"id-b\"\n",
    );
    CodexState::update(|state| {
        state.remove_profile("b");
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "profiles = [\n  # one\n  \"a\",\n  # two\n]\n\
         fallback_chain = [\"a\"]\n\n[ids]\n# ids note\na = \"id-a\"\n",
    );
}

#[test]
fn a_new_codex_profile_lands_in_the_list_and_its_id_table_in_place() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state("# keep\nprofiles = [\"a\"]\n\n[ids]\n# ids note\na = \"id-a\"\n");
    CodexState::update(|state| state.add_profile("b")).expect("update");
    let after = std::fs::read_to_string(&path).expect("read");
    assert!(
        after.starts_with(
            "# keep\nprofiles = [\"a\", \"b\"]\n\n[ids]\n# ids note\na = \"id-a\"\nb = \""
        ),
        "{after}"
    );
    let back = CodexState::load().expect("reload");
    assert_eq!(back.profiles().len(), 2);
    assert!(back.id("b").is_some());
}

/// An array of tables prints its key's decor above every element, so a carried
/// comment lands on the first element's header and appears once.
#[test]
fn a_removed_key_above_an_array_of_tables_hands_its_comment_to_the_first_element() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state(
        "profiles = [\"a\"]\n#c1\nactive_profile = \"a\" #c2\n\n#c9\n[[arr]]\nk = 1\n[[arr]]\nk = 2\n",
    );
    CodexState::update(|state| {
        state.set_active(None);
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "profiles = [\"a\"]\n#c1\n#c2\n\n#c9\n[[arr]]\nk = 1\n[[arr]]\nk = 2\n",
    );
}

#[test]
fn a_codex_update_with_no_file_writes_the_full_render() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    CodexState::update(|state| state.add_profile("a")).expect("update");
    let after = std::fs::read_to_string(&path).expect("read");
    assert!(after.contains("wrap_off = false"), "{after}");
    assert!(after.contains("fallback_chain = []"), "{after}");
}

/// The raw read sees the `nan` the load normalized away, so the typed view of
/// the file has to render it for the save to replace it.
#[test]
fn a_nan_weekly_line_heals_on_the_next_save() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state("profiles = [\"a\"]\nweekly_switch_threshold = nan\n");
    CodexState::update(|state| {
        state.set_active(Some("a"));
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "profiles = [\"a\"]\nweekly_switch_threshold = 98.0\nactive_profile = \"a\"\n",
    );
}

/// The load refuses a wrong-typed key first, so only a hand edit landing
/// between the in-hold load and the save's raw read reaches this refusal.
#[test]
fn a_wrong_typed_key_landing_inside_the_hold_refuses_the_save_naming_it() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state("profiles = [\"a\"]\n");
    let hand_edit = "# mine\nprofiles = \"oops\"\n";
    let err = CodexState::update(|state| {
        write_codex_state(hand_edit);
        state.set_active(Some("a"));
        Ok(())
    })
    .expect_err("the save refuses");
    assert_eq!(
        err.to_string(),
        "codex-profiles.toml has `profiles` in a shape clauth cannot read; \
         fix or remove that line, then try again",
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), hand_edit);
}

#[test]
fn a_file_turning_unparseable_inside_the_hold_refuses_the_save() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state("profiles = [\"a\"]\n");
    let hand_edit = "= nope\n";
    let err = CodexState::update(|state| {
        write_codex_state(hand_edit);
        state.set_active(Some("a"));
        Ok(())
    })
    .expect_err("the save refuses");
    assert_eq!(
        err.to_string(),
        format!("failed to parse {} at line 1, column 1", path.display()),
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), hand_edit);
}

#[test]
fn a_save_whose_edit_changes_no_bytes_leaves_the_mtime_alone() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state("profiles = [\"a\"]\n");
    let hand_edit = "profiles = [\"a\"] # mine\nactive_profile = \"a\"\n";
    let stamp = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    CodexState::update(|state| {
        write_codex_state(hand_edit);
        crate::testutil::set_mtime(&path, stamp);
        state.set_active(Some("a"));
        Ok(())
    })
    .expect("update");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), hand_edit);
    assert_eq!(
        std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        stamp,
    );
}

#[test]
fn keys_going_absent_leave_their_comments_on_the_next_item() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state(
        "# slot\nactive_profile = \"a\" # on it\n# marker\nswitched_off = true\n\
         # line\nweekly_switch_threshold = 75.0\nprofiles = [\"a\"]\n",
    );
    CodexState::update(|state| {
        state.set_active(None);
        state.clear_switched_off();
        state.weekly_switch_threshold = None;
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "# slot\n# on it\n# marker\n# line\nprofiles = [\"a\"]\n",
    );
}

#[test]
fn removing_the_last_codex_profile_moves_the_id_tables_comments() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state(
        "profiles = [\"a\"]\n\n# the ids\n[ids] # header\n# ids note\na = \"id-a\" # its own\n",
    );
    CodexState::update(|state| {
        state.remove_profile("a");
        Ok(())
    })
    .expect("update");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "profiles = []\n\n# the ids\n# header\n# ids note\n# its own\n",
    );
}

#[test]
fn an_id_table_the_file_lacks_lands_as_a_block_table() {
    let _home = HomeSandbox::new();
    let path = clauth_dir()
        .expect("clauth dir")
        .join("codex-profiles.toml");
    write_codex_state("profiles = [\"a\"] # mine\n");
    CodexState::update(|state| state.claim_id("a").map(|_| ())).expect("update");
    let id = CodexState::load().expect("reload").ids["a"].clone();
    let id = toml::Value::try_from(id).expect("id value");
    let id = id.as_str().expect("a string id");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        format!("profiles = [\"a\"] # mine\n\n[ids]\na = \"{id}\"\n"),
    );
}
