//! `clauth herdr install`'s decision half. `plan_config` is pure text in, text
//! out, which is where the append-only rule either holds or corrupts a config
//! clauth does not own; the subprocess half (herdr's installer, `config check`)
//! is covered by running the command against a real herdr. The `[herdr]` knob
//! table in profiles.toml and its `clauth herdr config get` read path are
//! pinned here too, through the same load the TUI uses.

use super::*;
use clap::{CommandFactory, Parser as _};

use crate::cli::{Cli, Command, HerdrCommand, HerdrConfigCommand};
use crate::profile::{HerdrSettings, PopupWidth};

/// Every plan this produces has to append onto the file it was planned against
/// and still parse, or the write turns a working herdr config into a broken one.
/// Goes through `with_append`, the same glue `install` uses, so the seam is
/// pinned here rather than reimplemented.
fn appended(existing: &str, key: &str, delegate_row_text: bool) -> String {
    let plan = plan_config(existing, key, delegate_row_text).expect("plan");
    let text = with_append(existing, &plan.append);
    toml::from_str::<toml::Value>(&text).expect("appended config parses");
    text
}

#[test]
fn empty_config_gets_both_blocks() {
    let text = appended("", "prefix+a", false);
    assert!(text.contains(r#"command = "clauth.open""#));
    assert!(text.contains(r#"key = "prefix+a""#));
    assert!(text.contains("[ui.sidebar.agents.rows_by_agent]"));
    assert!(text.contains("$clauth"));
}

#[test]
fn an_existing_binding_is_left_alone() {
    let existing = concat!(
        "[[keys.command]]\n",
        "key = \"prefix+z\"\n",
        "type = \"plugin_action\"\n",
        "command = \"clauth.open\"\n"
    );
    let plan = plan_config(existing, "prefix+a", false).expect("plan");
    assert!(
        !plan.append.contains("[[keys.command]]"),
        "would double-bind the action"
    );
    assert!(plan.notes.iter().any(|n| n.contains("already bound")));
    // The sidebar half is still missing, so the run is not a no-op.
    assert!(plan.append.contains("rows_by_agent"));
}

#[test]
fn another_plugins_binding_does_not_count_as_ours() {
    let existing = concat!(
        "[[keys.command]]\n",
        "key = \"prefix+g\"\n",
        "type = \"plugin_action\"\n",
        "command = \"someone.else\"\n"
    );
    let text = appended(existing, "prefix+a", false);
    assert!(text.contains(r#"command = "clauth.open""#));
    // Arrays of tables append cleanly, so both bindings survive.
    let doc: toml::Value = toml::from_str(&text).expect("parses");
    let commands = doc["keys"]["command"].as_array().expect("array");
    assert_eq!(commands.len(), 2);
}

#[test]
fn a_claude_row_already_rendering_the_token_is_left_alone() {
    let existing = concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["state_icon"], ["agent", "$clauth"]]"#,
        "\n"
    );
    let plan = plan_config(existing, "prefix+a", false).expect("plan");
    assert!(!plan.append.contains("rows_by_agent"));
    assert!(plan.notes.iter().any(|n| n.contains("already renders")));
}

/// The duplicate-table case: appending our own `[ui.sidebar.agents.rows_by_agent]`
/// beside theirs is a parse error, so the plan has to hand the line over instead.
#[test]
fn a_claude_row_without_the_token_is_reported_never_duplicated() {
    let existing = concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["state_icon"], ["agent"]]"#,
        "\n"
    );
    let plan = plan_config(existing, "prefix+a", false).expect("plan");
    assert!(
        !plan.append.contains("rows_by_agent"),
        "would duplicate the table"
    );
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("already sets a claude row"))
    );
    appended(existing, "prefix+a", false);
}

#[test]
fn a_rows_by_agent_table_for_other_agents_is_reported_never_duplicated() {
    let existing = concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"codex = [["state_icon"], ["agent"]]"#,
        "\n"
    );
    let plan = plan_config(existing, "prefix+a", false).expect("plan");
    assert!(
        !plan.append.contains("rows_by_agent"),
        "would duplicate the table"
    );
    assert!(plan.notes.iter().any(|n| n.contains("covers other agents")));
    appended(existing, "prefix+a", false);
}

/// `[ui.sidebar.agents]` existing without `rows_by_agent` is the common shape
/// (someone who set `row_gap`), and appending the child table is legal there.
#[test]
fn a_sidebar_agents_table_without_rows_by_agent_still_gets_the_block() {
    let existing = "[ui.sidebar.agents]\nrow_gap = 1\n";
    let text = appended(existing, "prefix+a", false);
    assert!(text.contains("[ui.sidebar.agents.rows_by_agent]"));
    let doc: toml::Value = toml::from_str(&text).expect("parses");
    assert_eq!(
        doc["ui"]["sidebar"]["agents"]["row_gap"].as_integer(),
        Some(1)
    );
    assert!(
        doc["ui"]["sidebar"]["agents"]["rows_by_agent"]
            .get("claude")
            .is_some()
    );
}

#[test]
fn a_fully_wired_config_plans_nothing() {
    let existing = appended("", "prefix+a", false);
    let plan = plan_config(&existing, "prefix+a", false).expect("plan");
    assert!(plan.append.is_empty(), "second run would append again");
    assert_eq!(plan.notes.len(), 2);
}

#[test]
fn a_config_that_does_not_parse_fails_before_anything_is_written() {
    assert!(plan_config("this is not toml", "prefix+a", false).is_err());
}

/// Comments and unrelated keys survive, because the write appends text rather
/// than reserializing a parsed document.
#[test]
fn unrelated_config_survives_verbatim() {
    let existing = "# my herdr config\n[ui]\naccent = \"cyan\"\n";
    let text = appended(existing, "prefix+a", false);
    assert!(text.starts_with(existing));
    assert!(text.contains("# my herdr config"));
}

#[test]
fn a_key_that_would_break_the_file_is_refused() {
    assert!(validate_key("prefix+a").is_ok());
    assert!(
        validate_key(r#"a" , x = ""#).is_err(),
        "a quote would escape the TOML string"
    );
    assert!(validate_key("a\\b").is_err());
    assert!(validate_key("a\nb").is_err());
    assert!(validate_key("").is_err());
    assert!(validate_key(&"x".repeat(65)).is_err());
}

/// The token search walks the row structure, so a rendering change in the toml
/// crate cannot silently turn "already wired" into "wire it again".
#[test]
fn token_detection_walks_nested_groups() {
    let row: toml::Value = toml::from_str(r#"v = [["a"], ["b", "$clauth"]]"#).expect("parse");
    assert!(mentions_token(&row["v"]));
    let plain: toml::Value = toml::from_str(r#"v = [["a"], ["b"]]"#).expect("parse");
    assert!(!mentions_token(&plain["v"]));
    let substring: toml::Value = toml::from_str(r#"v = ["$clauthx"]"#).expect("parse");
    assert!(
        !mentions_token(&substring["v"]),
        "a longer name is a different token"
    );
}

/// `with_append` is the seam between the plan and the file. A config that ends
/// mid-line would otherwise take the first appended line onto that line.
#[test]
fn the_append_seam_never_joins_two_lines() {
    assert_eq!(with_append("a = 1", "\n[b]\n"), "a = 1\n\n[b]\n");
    assert_eq!(with_append("a = 1\n", "\n[b]\n"), "a = 1\n\n[b]\n");
    assert_eq!(with_append("", "\n[b]\n"), "\n[b]\n");
    let joined = with_append(
        "accent = \"cyan\"",
        &plan_config("accent = \"cyan\"", "prefix+a", false)
            .expect("plan")
            .append,
    );
    toml::from_str::<toml::Value>(&joined).expect("a config with no trailing newline still parses");
}

/// Spellings that parse to the same shape but cannot be extended by appending a
/// header. Walking the parsed tree cannot tell these from an absent table, so
/// the plan has to try the text and hand the block over when it does not hold.
#[test]
fn a_table_spelled_inline_is_handed_over_never_appended_onto() {
    for existing in [
        r#"ui = { accent = "cyan" }"#,
        r#"keys = { }"#,
        "[keys.command]\nkey = \"prefix+z\"\n",
        r#"keys.command = [{ key = "prefix+z", type = "shell", command = "ls" }]"#,
        "[ui.sidebar.agents]\nrows_by_agent = { codex = [[\"agent\"]] }\n",
    ] {
        let plan = plan_config(existing, "prefix+a", false).expect("plan");
        let text = with_append(existing, &plan.append);
        toml::from_str::<toml::Value>(&text)
            .unwrap_or_else(|e| panic!("appending onto {existing:?} broke the config: {e}"));
        assert!(
            !plan.notes.is_empty(),
            "{existing:?} was silently left unwired with no note"
        );
    }
}

/// The refusal set is what `write_validated` bails on, so a complaint the config
/// already carried must never land in it.
#[test]
fn only_diagnostics_the_edit_added_are_refused() {
    let before = vec!["unknown config key accent".to_string()];
    let after = vec![
        "unknown config key accent".to_string(),
        "invalid keybinding: keys.command[0].key".to_string(),
    ];
    assert_eq!(
        added_diagnostics(&before, &after),
        vec!["invalid keybinding: keys.command[0].key"]
    );
    assert!(added_diagnostics(&before, &before).is_empty());
    assert!(
        added_diagnostics(&after, &before).is_empty(),
        "a complaint that went away is not ours"
    );
    let twice = vec!["same".to_string(), "same".to_string()];
    assert_eq!(
        added_diagnostics(&[], &twice),
        vec!["same"],
        "one line, reported once"
    );
}

/// herdr prints `<root>/plugins/config/<component>`; anything shorter is not
/// that shape, and guessing a root writes a config herdr never reads.
#[test]
fn the_config_root_is_derived_from_herdrs_own_path_or_refused() {
    assert_eq!(
        config_path_from_plugin_dir("/home/u/.config/herdr/plugins/config/clauth"),
        Some(std::path::PathBuf::from(
            "/home/u/.config/herdr/config.toml"
        ))
    );
    assert_eq!(
        config_path_from_plugin_dir(
            "/Users/u/Library/Application Support/herdr/plugins/config/clauth"
        ),
        Some(std::path::PathBuf::from(
            "/Users/u/Library/Application Support/herdr/config.toml"
        ))
    );
    assert_eq!(config_path_from_plugin_dir(""), None);
    assert_eq!(
        config_path_from_plugin_dir("/plugins/config/clauth"),
        None,
        "root has no config.toml of herdr's"
    );
    assert_eq!(config_path_from_plugin_dir("clauth"), None);
}

/// The seam `install` writes through: a plan appended onto its file must strip back to that file, byte for byte. `uninstall`'s strip is knob-agnostic, so the delegate row (knob on) has to round-trip the same as today's.
fn round_trips(orig: &str, delegate_row_text: bool) {
    let plan = plan_config(orig, "prefix+a", delegate_row_text).expect("plan");
    let text = with_append(orig, &plan.append);
    assert_eq!(without_marked_blocks(&text), orig, "round trip lost bytes");
}

#[test]
fn removing_marked_blocks_round_trips() {
    round_trips("", false);
    round_trips(
        "# my config\n[ui]\naccent = \"cyan\"\n[keys]\nleader = \"ctrl+a\"\n",
        false,
    );
    round_trips(
        "[[keys.command]]\nkey = \"prefix+z\"\ntype = \"shell\"\ncommand = \"ls\"\n",
        false,
    );
    round_trips("# my config\n[ui]\naccent = \"cyan\"\n", true);
}

#[test]
fn marked_blocks_mid_file_leave_trailing_content() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let trailing = format!("{wired}\n[extra]\nx = 1\n");
    assert_eq!(
        without_marked_blocks(&trailing),
        format!("{orig}\n[extra]\nx = 1\n")
    );
}

#[test]
fn nothing_marked_round_trips_unchanged() {
    let existing = "# my config\n[ui]\naccent = \"cyan\"\n[keys]\nleader = \"ctrl+a\"\n";
    assert_eq!(without_marked_blocks(existing), existing);
}

#[test]
fn registry_entry_from_reads_every_real_shape() {
    let linked = registry_entry_from(&plugin_list_json(LINKED)).expect("linked");
    assert!(linked.enabled);
    assert_eq!(linked.version.as_deref(), Some("0.1.0"));
    assert_eq!(linked.min_herdr_version.as_deref(), Some("0.8.0"));
    assert_eq!(
        linked.plugin_root.as_deref(),
        Some("/home/uwuclxdy/repos/rs/clauth/herdr-plugin")
    );
    assert_eq!(linked.source_kind.as_deref(), Some("local"));
    assert!(linked.warnings.is_empty());

    let github = registry_entry_from(&plugin_list_json(GITHUB)).expect("github");
    assert!(github.enabled);
    assert_eq!(github.source_kind.as_deref(), Some("github"));
    assert_eq!(github.plugin_root, None);
    assert!(github.warnings.is_empty());

    let disabled = registry_entry_from(&plugin_list_json(DISABLED)).expect("disabled");
    assert!(!disabled.enabled);
    assert_eq!(disabled.source_kind.as_deref(), Some("local"));

    let stale = registry_entry_from(&plugin_list_json(STALE)).expect("stale");
    assert!(stale.enabled);
    assert_eq!(
        stale.plugin_root.as_deref(),
        Some("/gone/clauth/herdr-plugin")
    );
    assert_eq!(
        stale.warnings,
        vec!["manifest unavailable: No such file or directory (os error 2)".to_string()]
    );

    assert!(registry_entry_from("garbage").is_none());
    assert!(registry_entry_from("").is_none());
    assert!(
        registry_entry_from(&plugin_list_json(r#"{"plugin_id":"other","enabled":true}"#)).is_none(),
        "another plugin is not ours"
    );
}

/// One version one above the crate's, built from `CARGO_PKG_VERSION` so a
/// version bump never reds these tests.
fn bumped_version() -> String {
    let mut parts: Vec<u32> = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|p| p.parse().expect("numeric version part"))
        .collect();
    *parts.last_mut().expect("non-empty version") += 1;
    parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

#[test]
fn plugin_update_needed_accepts_only_stale_github_entries() {
    let entry = |json: &str| registry_entry_from(&plugin_list_json(json)).expect("entry");
    let stale =
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github"}}"#;
    let current = format!(
        r#"{{"enabled":true,"version":"{}","plugin_id":"clauth","source":{{"kind":"github"}}}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let newer = format!(
        r#"{{"enabled":true,"version":"{}","plugin_id":"clauth","source":{{"kind":"github"}}}}"#,
        bumped_version()
    );

    assert!(
        plugin_update_needed(&entry(stale)),
        "a stale github entry updates"
    );
    assert!(
        !plugin_update_needed(&entry(&current)),
        "a current version is a no-op"
    );
    assert!(
        !plugin_update_needed(&entry(&newer)),
        "a newer install is never downgraded"
    );
    assert!(
        !plugin_update_needed(&entry(
            r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"local"}}"#
        )),
        "a linked checkout is the developer's live tree"
    );
    assert!(
        !plugin_update_needed(&entry(
            r#"{"enabled":false,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github"}}"#
        )),
        "never re-enable a deliberate disable"
    );
    assert!(
        !plugin_update_needed(&entry(
            r#"{"enabled":true,"version":"not-a-version","plugin_id":"clauth","source":{"kind":"github"}}"#
        )),
        "an unreadable version degrades to a no-op"
    );
    assert!(
        !plugin_update_needed(&entry(
            r#"{"enabled":true,"plugin_id":"clauth","source":{"kind":"github"}}"#
        )),
        "a missing version degrades to a no-op"
    );
}

/// A herdr shim whose `plugin list --json` answer is the `HEAL_ANSWER` env var,
/// and which logs every other invocation's argv into `heal.log` beside itself.
/// The heal's install lands in that log; a no-op leaves the file absent.
#[cfg(unix)]
fn heal_shim(dir: &Path) -> PathBuf {
    write_shim(
        dir,
        "herdr",
        "if [ \"$1\" = \"plugin\" ] && [ \"$2\" = \"list\" ]; then echo \"$HEAL_ANSWER\"; exit 0; fi; echo \"$@\" >> \"$(dirname \"$0\")/heal.log\"; exit 0",
    )
}

/// A herdr shim for the stamp path: `plugin list --json` answers
/// `HEAL_ANSWER_BEFORE` until an install runs, then `HEAL_ANSWER_AFTER` (the
/// flip is an `installed` state file the install leg creates). Every other
/// invocation logs its argv into `heal.log` beside itself, like [`heal_shim`].
#[cfg(unix)]
fn stateful_heal_shim(dir: &Path) -> PathBuf {
    write_shim(
        dir,
        "herdr",
        "if [ \"$1\" = \"plugin\" ] && [ \"$2\" = \"list\" ]; then if [ -f \"$(dirname \"$0\")/installed\" ]; then echo \"$HEAL_ANSWER_AFTER\"; else echo \"$HEAL_ANSWER_BEFORE\"; fi; exit 0; fi; if [ \"$1\" = \"plugin\" ] && [ \"$2\" = \"install\" ]; then : > \"$(dirname \"$0\")/installed\"; fi; echo \"$@\" >> \"$(dirname \"$0\")/heal.log\"; exit 0",
    )
}

#[cfg(unix)]
#[test]
fn plugin_heal_reinstalls_a_stale_github_install() {
    let home = crate::testutil::HomeSandbox::new();
    let stale = plugin_list_json(
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}"#,
    );
    let shim = heal_shim(home.home());
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
            ),
            ("HEAL_ANSWER", Some(std::ffi::OsStr::new(&stale))),
        ],
    );

    let line = plugin_heal_line()
        .expect("heal runs")
        .expect("update lands");
    assert!(
        line.contains("reinstalled the herdr plugin from uwuclxdy/clauth/herdr-plugin"),
        "the line names the reinstall: {line}"
    );
    assert!(
        line.contains("was 0.0.0"),
        "the line names the installed version: {line}"
    );

    let log = std::fs::read_to_string(home.home().join("heal.log")).unwrap_or_default();
    assert_eq!(
        log.trim(),
        "plugin install uwuclxdy/clauth/herdr-plugin --yes",
        "the update is a reinstall with the preview skipped"
    );
}

/// The stamp half of the heal: a reinstall that still trails the binary gets
/// its installed manifest's version line rewritten, and the success line says
/// so. The shim flips its registry answer on the install's state file, so the
/// pre-install and post-install probes read different entries.
#[cfg(unix)]
#[test]
fn plugin_heal_stamps_a_still_trailing_installed_manifest() {
    let home = crate::testutil::HomeSandbox::new();
    let planted = home.home().join("installed-plugin");
    std::fs::create_dir_all(&planted).expect("planted dir");
    let manifest = planted.join("herdr-plugin.toml");
    let fixture =
        "id = \"clauth\"\nname = \"clauth\"\nversion = \"0.0.0\"\nmin_herdr_version = \"0.8.0\"\n";
    std::fs::write(&manifest, fixture).expect("fixture written");

    let before = plugin_list_json(
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}"#,
    );
    let after = plugin_list_json(&format!(
        r#"{{"enabled":true,"version":"0.0.0","plugin_id":"clauth","plugin_root":"{}","source":{{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}}}"#,
        planted.display()
    ));
    let shim = stateful_heal_shim(home.home());
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
            ),
            ("HEAL_ANSWER_BEFORE", Some(std::ffi::OsStr::new(&before))),
            ("HEAL_ANSWER_AFTER", Some(std::ffi::OsStr::new(&after))),
        ],
    );

    let line = plugin_heal_line()
        .expect("heal runs")
        .expect("update lands");
    assert!(
        line.contains("stamped the manifest version to"),
        "the line names the stamp: {line}"
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).expect("manifest reads"),
        format!(
            "id = \"clauth\"\nname = \"clauth\"\nversion = \"{}\"\nmin_herdr_version = \"0.8.0\"\n",
            crate::update::CURRENT_VERSION
        ),
        "the version line carries the crate version, everything else byte-for-byte"
    );
}

/// The stamp's control: when the fresh post-install entry already carries the
/// crate version, the heal leaves the planted manifest byte-identical and the
/// line carries no stamp clause.
#[cfg(unix)]
#[test]
fn plugin_heal_leaves_a_current_manifest_byte_identical_after_reinstall() {
    let home = crate::testutil::HomeSandbox::new();
    let planted = home.home().join("installed-plugin");
    std::fs::create_dir_all(&planted).expect("planted dir");
    let manifest = planted.join("herdr-plugin.toml");
    let fixture = "id = \"clauth\"\nversion = \"0.0.0\"\n";
    std::fs::write(&manifest, fixture).expect("fixture written");

    let before = plugin_list_json(
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}"#,
    );
    let after = plugin_list_json(&format!(
        r#"{{"enabled":true,"version":"{}","plugin_id":"clauth","plugin_root":"{}","source":{{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}}}"#,
        env!("CARGO_PKG_VERSION"),
        planted.display()
    ));
    let shim = stateful_heal_shim(home.home());
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
            ),
            ("HEAL_ANSWER_BEFORE", Some(std::ffi::OsStr::new(&before))),
            ("HEAL_ANSWER_AFTER", Some(std::ffi::OsStr::new(&after))),
        ],
    );

    let line = plugin_heal_line()
        .expect("heal runs")
        .expect("update lands");
    assert!(
        !line.contains("stamped the manifest version to"),
        "a current entry is not stamped: {line}"
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).expect("manifest reads"),
        fixture,
        "no stamp: the manifest stays byte-identical"
    );
}

/// `install` over a checkout stamps the linked manifest before `plugin link`,
/// so a linked checkout reports the running binary's version to herdr.
#[cfg(unix)]
#[test]
fn install_links_a_checkout_and_stamps_its_manifest() {
    let home = crate::testutil::HomeSandbox::new();
    let checkout = home.home().join("checkout");
    let plugin_dir = checkout.join("herdr-plugin");
    std::fs::create_dir_all(&plugin_dir).expect("checkout dir");
    let manifest = plugin_dir.join("herdr-plugin.toml");
    std::fs::write(&manifest, "id = \"clauth\"\nversion = \"0.0.0\"\n").expect("fixture written");

    let shim = write_shim(
        home.home(),
        "herdr",
        "echo \"$@\" >> \"$(dirname \"$0\")/link.log\"; exit 0",
    );
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[(
            "HERDR_BIN_PATH",
            Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
        )],
    );

    {
        let _cwd = CwdPin::new(&checkout);
        install(None, true, true, false).expect("install links the checkout");
    }

    let text = std::fs::read_to_string(&manifest).expect("manifest reads");
    assert!(
        text.contains(&format!("version = \"{}\"", crate::update::CURRENT_VERSION)),
        "the checkout's manifest carries the crate version: {text}"
    );
    let log = std::fs::read_to_string(home.home().join("link.log")).unwrap_or_default();
    assert!(
        log.trim().starts_with("plugin link "),
        "the link ran after the stamp: {log}"
    );
    assert!(
        log.trim().ends_with("/checkout/herdr-plugin"),
        "the link names the checkout's plugin dir: {log}"
    );
}

#[cfg(unix)]
#[test]
fn plugin_heal_skips_every_non_stale_shape() {
    let home = crate::testutil::HomeSandbox::new();
    let shim = heal_shim(home.home());
    let _bin = crate::testutil::EnvPin::new(
        &home,
        &[(
            "HERDR_BIN_PATH",
            Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
        )],
    );

    let current = format!(
        r#"{{"enabled":true,"version":"{}","plugin_id":"clauth","source":{{"kind":"github"}}}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let newer = format!(
        r#"{{"enabled":true,"version":"{}","plugin_id":"clauth","source":{{"kind":"github"}}}}"#,
        bumped_version()
    );
    let cases = [
        (
            "no clauth entry",
            plugin_list_json(r#"{"plugin_id":"other"}"#),
        ),
        (
            "a linked checkout",
            plugin_list_json(
                r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"local"}}"#,
            ),
        ),
        (
            "a disabled entry",
            plugin_list_json(
                r#"{"enabled":false,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github"}}"#,
            ),
        ),
        ("a current version", plugin_list_json(&current)),
        ("a newer version", plugin_list_json(&newer)),
    ];
    for (why, answer) in cases {
        let _answer = crate::testutil::EnvPin::new(
            &home,
            &[("HEAL_ANSWER", Some(std::ffi::OsStr::new(&answer)))],
        );
        assert!(
            plugin_heal_line().expect("heal runs").is_none(),
            "case `{why}` must be a no-op"
        );
    }
    assert!(
        !home.home().join("heal.log").exists(),
        "none of the no-op cases installed anything"
    );
}

#[cfg(unix)]
#[test]
fn plugin_heal_fails_loud_on_a_broken_probe() {
    let home = crate::testutil::HomeSandbox::new();
    let shim = write_shim(home.home(), "herdr", "echo 'herdr is sick' >&2; exit 1");
    let _bin = crate::testutil::EnvPin::new(
        &home,
        &[(
            "HERDR_BIN_PATH",
            Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
        )],
    );

    let err = plugin_heal_line().expect_err("a broken probe must fail, not skip");
    assert!(
        format!("{err:#}").contains("plugin list"),
        "the error names the failing probe: {err:#}"
    );
}

/// The install bound: a herdr that never answers costs the caller the timeout,
/// never a wedge — the heal's in-flight claim must not sit on a stalled fetch.
#[cfg(unix)]
#[test]
fn a_stalled_install_bounds_the_heal() {
    let home = crate::testutil::HomeSandbox::new();
    let shim = write_shim(home.home(), "herdr", "exec sleep 10");
    let start = std::time::Instant::now();
    let err = run_quiet_bounded(
        shim.to_str().expect("utf8 path"),
        &["plugin", "install", "uwuclxdy/clauth/herdr-plugin", "--yes"],
        std::time::Duration::from_secs(1),
    )
    .expect_err("a stalled install must fail, not hang");
    assert!(
        format!("{err:#}").contains("timed out after 1s"),
        "the error names the bound: {err:#}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "the caller is bounded, not the shim's full sleep"
    );
}

/// The heal-level twin of the helper pin above: `plugin_heal_line` must route
/// its install through the bound, not just `run_quiet_bounded` exist. A bound
/// dropped from the heal makes this red after the shim's full sleep, since the
/// unbounded run exits 0 where the bounded one reports the timeout.
#[cfg(unix)]
#[test]
fn plugin_heal_bounds_a_stalled_install() {
    let home = crate::testutil::HomeSandbox::new();
    let stale = plugin_list_json(
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}"#,
    );
    let shim = write_shim(
        home.home(),
        "herdr",
        "if [ \"$1\" = \"plugin\" ] && [ \"$2\" = \"list\" ]; then echo \"$HEAL_ANSWER\"; exit 0; fi; exec sleep 10",
    );
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
            ),
            ("HEAL_ANSWER", Some(std::ffi::OsStr::new(&stale))),
        ],
    );

    let start = std::time::Instant::now();
    let err = plugin_heal_line_with(std::time::Duration::from_secs(1))
        .expect_err("a stalled install must fail the heal, not hang");
    assert!(
        format!("{err:#}").contains("timed out after 1s"),
        "the error names the bound: {err:#}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "the heal is bounded, not the shim's full sleep"
    );
}

#[cfg(unix)]
#[test]
fn heal_detached_reinstalls_once_and_throttles() {
    use crate::testutil::join_background_tasks;

    let home = crate::testutil::HomeSandbox::new();
    let stale = plugin_list_json(
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}"#,
    );
    let shim = heal_shim(home.home());
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
            ),
            ("HEAL_ANSWER", Some(std::ffi::OsStr::new(&stale))),
            ("HERDR_SHIM_STATE", Some(std::ffi::OsStr::new("1"))),
        ],
    );
    reset_heal_throttle_for_test();

    heal_detached();
    join_background_tasks();
    let log = std::fs::read_to_string(home.home().join("heal.log")).unwrap_or_default();
    assert_eq!(
        log.trim(),
        "plugin install uwuclxdy/clauth/herdr-plugin --yes",
        "the first attempt installs"
    );

    // The floor is armed by the first attempt: a second spawns nothing.
    heal_detached();
    join_background_tasks();
    let log = std::fs::read_to_string(home.home().join("heal.log")).unwrap_or_default();
    assert_eq!(
        log.trim(),
        "plugin install uwuclxdy/clauth/herdr-plugin --yes",
        "the floor refuses a second attempt"
    );
}

#[cfg(unix)]
#[test]
fn heal_detached_respects_the_update_optout() {
    use crate::testutil::join_background_tasks;

    let home = crate::testutil::HomeSandbox::new();
    let stale = plugin_list_json(
        r#"{"enabled":true,"version":"0.0.0","plugin_id":"clauth","source":{"kind":"github","owner":"uwuclxdy","repo":"clauth"}}"#,
    );
    let shim = heal_shim(home.home());
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(std::ffi::OsStr::new(shim.to_str().expect("utf8 path"))),
            ),
            ("HEAL_ANSWER", Some(std::ffi::OsStr::new(&stale))),
            ("CLAUTH_NO_UPDATE", Some(std::ffi::OsStr::new("1"))),
            ("HERDR_SHIM_STATE", Some(std::ffi::OsStr::new("1"))),
        ],
    );
    reset_heal_throttle_for_test();

    heal_detached();
    join_background_tasks();
    assert!(
        !home.home().join("heal.log").exists(),
        "the opt-out gates the network update"
    );
}

/// The fail-closed sentinel: `heal_detached` refuses to run when only
/// `HERDR_BIN_PATH` is pinned, because a herdr pane injects that with the
/// operator's real binary. The shim-state var is the sentinel a test fake sets.
#[cfg(unix)]
#[test]
#[should_panic(expected = "stage a herdr shim beside a `HERDR_SHIM_STATE` pin")]
fn heal_detached_fails_closed_without_the_shim_sentinel() {
    let home = crate::testutil::HomeSandbox::new();
    reset_heal_throttle_for_test();
    let _env = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                "HERDR_BIN_PATH",
                Some(home.home().join("no-such-herdr").as_os_str()),
            ),
            ("HERDR_SHIM_STATE", None),
            ("CLAUTH_NO_UPDATE", None),
        ],
    );

    heal_detached();
}

#[test]
fn version_from_parses_the_real_line() {
    assert_eq!(version_from("herdr 0.8.0"), Some("0.8.0".to_string()));
    assert_eq!(version_from("herdr 0.8.0\n"), Some("0.8.0".to_string()));
    assert_eq!(version_from("herdr"), None);
    assert_eq!(version_from("0.8.0"), None);
    assert_eq!(version_from(""), None);
}

#[test]
fn config_status_reads_binding_and_sidebar() {
    let empty = config_status("");
    assert!(empty.parsed);
    assert_eq!(empty.bound_key, None);
    assert_eq!(empty.sidebar, SidebarState::Absent);

    let bound_other_key = config_status(concat!(
        "[[keys.command]]\n",
        "key = \"ctrl+alt+c\"\n",
        "type = \"plugin_action\"\n",
        "command = \"clauth.open\"\n",
    ));
    assert_eq!(bound_other_key.bound_key.as_deref(), Some("ctrl+alt+c"));
    assert_eq!(bound_other_key.sidebar, SidebarState::Absent);

    let templated = config_status(concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["agent", "$clauth"]]"#,
        "\n",
    ));
    assert_eq!(templated.sidebar, SidebarState::Templated);

    let other_claude = config_status(concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["agent"]]"#,
        "\n",
    ));
    assert_eq!(other_claude.sidebar, SidebarState::OtherClaudeRow);

    let other_agents = config_status(concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"codex = [["agent"]]"#,
        "\n",
    ));
    assert_eq!(other_agents.sidebar, SidebarState::OtherAgentsOnly);

    let broken = config_status("not toml");
    assert!(!broken.parsed);
    assert_eq!(broken.bound_key, None);
    assert_eq!(broken.sidebar, SidebarState::Absent);
}

#[test]
fn plan_config_and_config_status_agree() {
    let configs: &[&str] = &[
        "",
        "[[keys.command]]\nkey = \"prefix+z\"\ntype = \"shell\"\ncommand = \"ls\"\n",
        "[[keys.command]]\nkey = \"ctrl+alt+c\"\ntype = \"plugin_action\"\ncommand = \"clauth.open\"\n",
        "[ui.sidebar.agents.rows_by_agent]\nclaude = [[\"agent\", \"$clauth\"]]\n",
        "[ui.sidebar.agents.rows_by_agent]\nclaude = [[\"agent\"]]\n",
        "[ui.sidebar.agents.rows_by_agent]\ncodex = [[\"agent\"]]\n",
        "[ui.sidebar.agents]\nrow_gap = 1\n",
    ];
    for existing in configs {
        let status = config_status(existing);
        let plan = plan_config(existing, "prefix+a", false).expect("plan");
        assert_eq!(
            status.bound_key.is_some(),
            plan.notes.iter().any(|n| n.contains("already bound")),
            "binding verdicts drifted for {existing:?}"
        );
        assert_eq!(
            status.sidebar == SidebarState::Templated,
            plan.notes.iter().any(|n| n.contains("already renders")),
            "templated verdicts drifted for {existing:?}"
        );
        assert_eq!(
            status.sidebar == SidebarState::OtherClaudeRow,
            plan.notes
                .iter()
                .any(|n| n.contains("already sets a claude row")),
            "other-claude verdicts drifted for {existing:?}"
        );
        assert_eq!(
            status.sidebar == SidebarState::OtherAgentsOnly,
            plan.notes.iter().any(|n| n.contains("covers other agents")),
            "other-agents verdicts drifted for {existing:?}"
        );
    }
}

#[test]
fn a_marker_as_the_last_line_drops_alone() {
    let existing = "# my config\n[ui]\naccent = \"cyan\"\n# clauth herdr plugin";
    assert_eq!(
        without_marked_blocks(existing),
        "# my config\n[ui]\naccent = \"cyan\"\n"
    );
}

#[test]
fn a_marker_before_user_content_keeps_the_content() {
    let existing = "[misc]\n# clauth herdr plugin\n\nsomething = \"user value\"\n";
    assert_eq!(
        without_marked_blocks(existing),
        "[misc]\n\nsomething = \"user value\"\n"
    );
}

#[test]
fn bound_key_reads_the_open_binding_not_the_first_entry() {
    let doc: toml::Value = toml::from_str(concat!(
        "[[keys.command]]\n",
        "key = \"prefix+z\"\n",
        "type = \"shell\"\n",
        "command = \"ls\"\n",
        "\n",
        "[[keys.command]]\n",
        "key = \"prefix+a\"\n",
        "type = \"plugin_action\"\n",
        "command = \"clauth.open\"\n",
    ))
    .expect("parses");
    assert_eq!(bound_key(&doc).as_deref(), Some("prefix+a"));
}

#[test]
fn a_registry_entry_without_enabled_reads_as_enabled() {
    let entry = registry_entry_from(&plugin_list_json(
        r#"{"plugin_id":"clauth","version":"0.1.0"}"#,
    ))
    .expect("entry");
    assert!(entry.enabled);
}

#[test]
fn registry_warnings_skip_non_strings_without_panicking() {
    let entry = registry_entry_from(&plugin_list_json(
        r#"{"plugin_id":"clauth","warnings":["a",1,"b"]}"#,
    ))
    .expect("entry");
    assert_eq!(entry.warnings, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn version_from_stops_at_the_first_token() {
    assert_eq!(
        version_from("herdr 0.8.0 (abcdef)"),
        Some("0.8.0".to_string())
    );
}

#[test]
fn read_config_treats_absent_as_empty_and_fails_on_non_utf8() {
    let sandbox = crate::testutil::HomeSandbox::new();

    let absent = sandbox.home().join("no-such.toml");
    assert_eq!(read_config(&absent).expect("absent reads empty"), "");

    let garbage = sandbox.home().join("garbage.toml");
    std::fs::write(&garbage, [0xFF, 0xFE, 0x00, 0x00]).expect("write non-utf-8");
    let err = read_config(&garbage).expect_err("non-utf-8 fails, not empty");
    assert!(
        format!("{err:#}").contains("garbage.toml"),
        "names the path: {err:#}"
    );
}

// ── delegate_row_text knob: the row content + the install/heal resync ─────

/// The knob's only effect on the plan: off writes today's row byte for byte,
/// on appends the delegate token to the agent group and nothing else.
#[test]
fn the_delegate_token_rides_the_row_only_when_the_knob_is_on() {
    let off = appended("", "prefix+a", false);
    assert!(
        off.contains(r#"["agent", "$clauth"]"#),
        "the knob off writes today's row exactly: {off}"
    );
    assert!(
        !off.contains("$clauth_delegate"),
        "the delegate token must stay out of the off row: {off}"
    );

    let on = appended("", "prefix+a", true);
    assert!(
        on.contains(r#"["agent", "$clauth", "$clauth_delegate"]"#),
        "the knob on appends the delegate token to the agent group: {on}"
    );
    // The row is otherwise identical.
    let off_row = off.split("rows_by_agent]\n").nth(1).expect("row");
    let on_row = on.split("rows_by_agent]\n").nth(1).expect("row");
    assert_eq!(
        on_row.replace(
            r#"["agent", "$clauth", "$clauth_delegate"]"#,
            r#"["agent", "$clauth"]"#
        ),
        off_row,
        "the on row differs only in the agent group"
    );
}

/// The resync `install`/`heal` run, through the seam they both write through:
/// strip clauth's blocks, plan on the base, append. A knob toggle must
/// rewrite exactly the blocks clauth wrote — the old row goes, the new one
/// lands, nothing user-owned moves — and the toggle back restores the
/// original byte for byte.
#[test]
fn a_knob_toggle_rewrites_exactly_the_marked_blocks() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    // First run, knob off — today's row.
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);

    // The resync a toggle run makes, with the knob on.
    let (text, plan, _) = resync_text(&wired, "prefix+a", true).expect("resync");
    assert!(
        plan.notes.is_empty(),
        "the base the strip left carries nothing hand-owned to report on"
    );
    toml::from_str::<toml::Value>(&text).expect("the rewritten config parses");
    assert!(
        text.starts_with(orig),
        "the user's own content is untouched"
    );
    assert_eq!(
        text.matches("$clauth_delegate").count(),
        1,
        "exactly one delegate token, the new row's: {text}"
    );
    assert_eq!(
        text.matches("rows_by_agent").count(),
        1,
        "the old row was stripped, not duplicated: {text}"
    );
    assert_eq!(
        text.matches("[[keys.command]]").count(),
        1,
        "one binding block survives the strip-and-replan"
    );

    // And the toggle back restores today's row byte for byte.
    let (back, _, _) = resync_text(&text, "prefix+a", false).expect("resync back");
    assert_eq!(back, wired, "off -> on -> off round-trips byte for byte");
}

/// A user key glued directly onto the last line install wrote (no blank
/// separator, valid TOML in the same table) is not part of any block clauth
/// wrote: the strip must end the block before it, so the resync write cannot
/// eat the line.
#[test]
fn a_user_key_glued_to_a_marked_block_survives_the_resync() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let glued = format!("{wired}row_gap = 1\n");
    toml::from_str::<toml::Value>(&glued).expect("the glued line is valid TOML");
    let (text, _, _) = resync_text(&glued, "prefix+a", false).expect("resync");
    toml::from_str::<toml::Value>(&text).expect("the rewritten config parses");
    assert!(
        text.contains("row_gap = 1"),
        "the user's glued line survives the resync write: {text}"
    );
    assert_eq!(
        text.matches("[[keys.command]]").count(),
        1,
        "the binding block stays in place exactly once: {text}"
    );
}

/// A user line INTERRUPTING clauth's block (a comment or key between the
/// block's own lines) makes the whole block user-owned: the strip must keep
/// the header and every line, or the resync write strands the tail lines
/// under a table whose header was consumed.
#[test]
fn a_user_line_inside_a_marked_block_keeps_the_whole_block() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    // Interrupt the keys block: a comment between key and type.
    let interrupted = wired.replace(
        "key = \"prefix+a\"\ntype = \"plugin_action\"",
        "key = \"prefix+a\"\n# pinned by hand\ntype = \"plugin_action\"",
    );
    let (text, _, _) = resync_text(&interrupted, "prefix+a", false).expect("resync");
    toml::from_str::<toml::Value>(&text).expect("the rewritten config parses");
    // The discriminator that matters: the whole interrupted block survives
    // under its own header. A strip that removes the header while keeping the
    // tail strands `type`/`command`/`description` under `[ui]` — this
    // sequence check catches that, a parse check alone does not.
    assert!(
        text.contains(
            "[[keys.command]]\nkey = \"prefix+a\"\n# pinned by hand\ntype = \"plugin_action\""
        ),
        "the interrupted block stays intact under its header: {text}"
    );
    assert_eq!(
        text.matches("[[keys.command]]").count(),
        1,
        "the interrupted block is not duplicated: {text}"
    );
}

// ── edited marked blocks: the strip keeps what clauth did not write ────────

/// The todo's repro: a user who trims the claude row (drops the `tab` group)
/// owns that row now. The strip must compare the block against every block
/// clauth writes — both knob variants — and keep a mismatch whole, so the
/// resync reconstructs the file byte for byte and the plan reports the row
/// through the hand-owned note instead of silently rewriting it. The toggle
/// direction holds too: an edited row matches neither variant, so a knob
/// change leaves it alone just the same.
#[test]
fn an_edited_sidebar_row_survives_the_resync_byte_for_byte() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let edited = wired.replace(
        r#"claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$clauth"]]"#,
        r#"claude = [["state_icon", "workspace"], ["agent", "$clauth"]]"#,
    );
    assert_ne!(edited, wired, "the fixture edit landed");
    for delegate_row_text in [false, true] {
        let (text, plan, removed, noop) =
            install_resync(&edited, "prefix+a", delegate_row_text).expect("resync");
        assert!(
            !removed.iter().any(|line| line.starts_with("claude = ")),
            "the edited row is not in the removal diff: {removed:?}"
        );
        assert_eq!(
            text, edited,
            "the edited row survives a resync byte for byte (knob {delegate_row_text})"
        );
        assert!(
            noop,
            "an edited row is left alone, so the heal write is skipped (knob {delegate_row_text})"
        );
        assert!(
            plan.notes.iter().any(|n| n.contains("already renders")),
            "the row still renders the token, so the hand-owned note fires (knob {delegate_row_text}): {:?}",
            plan.notes
        );
    }
}

/// A binding block edited without touching the command it binds (a
/// description, say) is the user's now: the strip keeps the block, the plan
/// sees `clauth.open` still bound and reports it, and the resync
/// reconstructs the file byte for byte.
#[test]
fn an_edited_binding_block_survives_the_resync_byte_for_byte() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let edited = wired.replace(
        r#"description = "clauth accounts""#,
        r#"description = "opener""#,
    );
    assert_ne!(edited, wired, "the fixture edit landed");
    let (text, plan, removed, noop) = install_resync(&edited, "prefix+a", false).expect("resync");
    assert!(
        !removed.iter().any(|line| line.starts_with("command = ")),
        "the edited binding is not in the removal diff: {removed:?}"
    );
    assert_eq!(text, edited, "the edited binding survives byte for byte");
    assert!(
        noop,
        "an edited binding is left alone, so the heal write is skipped"
    );
    assert!(
        plan.notes.iter().any(|n| n.contains("already bound")),
        "`clauth.open` is still bound, so the hand-owned note fires: {:?}",
        plan.notes
    );
}

/// An edit that moves the command off `clauth.open` (say `clauth.open
/// --now`) keeps the whole block too, and `bound_key` matches the exact
/// command, so `clauth.open` now reads as unbound: no already-bound note
/// fires, and clauth appends its own binding beside the user's.
#[test]
fn a_binding_edited_off_clauth_open_keeps_the_edit_and_rewires_clauths_own() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let edited = wired.replace(
        r#"command = "clauth.open""#,
        r#"command = "clauth.open --now""#,
    );
    assert_ne!(edited, wired, "the fixture edit landed");
    let (text, plan, removed, noop) = install_resync(&edited, "prefix+a", false).expect("resync");
    assert!(
        !removed.iter().any(|line| line.starts_with("command = ")),
        "the edited binding is not in the removal diff: {removed:?}"
    );
    assert!(
        text.contains(r#"command = "clauth.open --now""#),
        "the user's command edit survives: {text}"
    );
    assert!(
        text.contains(r#"command = "clauth.open""#),
        "`clauth.open` reads as unbound, so clauth wires its own binding"
    );
    assert_eq!(
        text.matches("[[keys.command]]").count(),
        2,
        "the edited binding and clauth's own both stand: {text}"
    );
    assert!(
        plan.notes.iter().all(|n| !n.contains("already bound")),
        "no already-bound note: the edited command no longer binds `clauth.open`: {:?}",
        plan.notes
    );
    assert!(!noop, "clauth's own binding is added, so the write fires");
}

/// `uninstall` strips with no key in hand, and it must keep the user's edits
/// just the same: an edited sidebar block is absent from the removal diff,
/// while an untouched binding block still comes out.
#[test]
fn an_edited_block_is_absent_from_uninstalls_removal_diff() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let edited = wired.replace(
        r#"claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$clauth"]]"#,
        r#"claude = [["state_icon", "workspace"], ["agent", "$clauth"]]"#,
    );
    assert_ne!(edited, wired, "the fixture edit landed");
    let (text, removed, kept_after_stripped) = strip_marked_blocks(&edited);
    assert!(
        kept_after_stripped,
        "the strip reports a kept block following a stripped one"
    );
    assert!(
        removed
            .iter()
            .any(|line| line.starts_with("[[keys.command]]")),
        "the untouched binding block still comes out: {removed:?}"
    );
    assert!(
        !removed.iter().any(|line| line.starts_with("claude = ")),
        "the edited sidebar block is not in the removal diff: {removed:?}"
    );
    assert!(
        text.contains(r#"claude = [["state_icon", "workspace"], ["agent", "$clauth"]]"#),
        "the edited row survives the uninstall strip: {text}"
    );
}

/// A config installed under a custom key: the TUI heal always plans with
/// the default key, and it must still land a knob toggle while keeping the
/// key the user installed under. Drives the real `heal` (a shim herdr
/// accepts the validated write), since the key re-use lives in `heal` itself.
#[cfg(unix)]
#[test]
fn a_heal_keeps_a_custom_key_binding_and_still_toggles_the_row() {
    let home = crate::testutil::HomeSandbox::new();
    let herdr = write_shim(home.home(), "herdr", "exit 0");
    let path = home.home().join("config.toml");
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "ctrl+alt+x", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    std::fs::write(&path, &wired).expect("fixture written");
    let notes = heal(&path, DEFAULT_KEY, herdr.to_str().expect("path"), true).expect("heal");
    assert!(
        notes.is_empty(),
        "nothing hand-owned in the fixture: {notes:?}"
    );
    let text = std::fs::read_to_string(&path).expect("read back");
    assert!(
        text.contains(r#"key = "ctrl+alt+x""#),
        "the heal keeps the installed key: {text}"
    );
    assert!(
        !text.contains(r#"key = "prefix+a""#),
        "the heal does not re-key the binding: {text}"
    );
    assert!(
        text.contains("$clauth_delegate"),
        "the knob toggle still lands: {text}"
    );
}

/// `install --key <new>` over a config wired under another key re-binds:
/// the binding comparison runs modulo the key, so the strip takes the old
/// block and the plan re-adds it under the key just passed.
#[test]
fn an_install_with_a_new_key_rebinds_a_wired_config() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "ctrl+alt+x", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let (text, plan, _, _) = install_resync(&wired, "prefix+z", false).expect("resync");
    assert!(
        plan.notes.is_empty(),
        "no hand-owned pieces: {:?}",
        plan.notes
    );
    assert!(
        text.contains(r#"key = "prefix+z""#),
        "the new key lands: {text}"
    );
    assert!(
        !text.contains(r#"key = "ctrl+alt+x""#),
        "the old key goes: {text}"
    );
    assert_eq!(
        text.matches("[[keys.command]]").count(),
        1,
        "one binding, rebound: {text}"
    );
}

/// An edited binding is kept but does not freeze the untouched blocks after
/// it: the plan appends at the end, and re-appending the stripped sidebar
/// block lands it in its original slot (binding first, sidebar last). The
/// knob toggle must still rewrite the sidebar row.
#[test]
fn an_edited_binding_does_not_freeze_the_sidebar_toggle() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let edited = wired.replace(
        r#"description = "clauth accounts""#,
        r#"description = "opener""#,
    );
    assert_ne!(edited, wired, "the fixture edit landed");
    let (text, plan, _, _) = install_resync(&edited, "prefix+a", true).expect("resync");
    assert!(
        text.contains(r#"description = "opener""#),
        "the edited binding survives: {text}"
    );
    assert!(
        text.contains("$clauth_delegate"),
        "the sidebar toggle still lands beside the kept binding: {text}"
    );
    assert_eq!(
        text.matches("[[keys.command]]").count(),
        1,
        "the kept binding is not duplicated: {text}"
    );
    assert!(
        plan.notes.iter().any(|n| n.contains("already bound")),
        "the kept binding keeps its already-bound note: {:?}",
        plan.notes
    );
}

/// `install`'s no-op branch: the verdict skips the write only when the resync
/// reconstructs the file byte for byte. Pinned both ways, since the branch
/// lives where a TTY and herdr's installer keep tests out.
#[test]
fn the_noop_verdict_holds_both_ways() {
    let orig = "# my config\n[ui]\naccent = \"cyan\"\n";
    let plan = plan_config(orig, "prefix+a", false).expect("plan");
    let wired = with_append(orig, &plan.append);
    let (_, _, _, noop) = install_resync(&wired, "prefix+a", false).expect("resync");
    assert!(noop, "an unchanged knob is a no-op");

    // The toggle direction: the write must fire, or the old row survives.
    let (_, _, _, noop) = install_resync(&wired, "prefix+a", true).expect("resync");
    assert!(!noop, "a knob toggle is never a no-op");
}

/// Runs `sed -n <program>` over `input` and returns its stdout. The fit
/// branch's parse runs through the installed sed, so the fixture pins the
/// pattern against real sed semantics rather than a rust re-implementation.
#[cfg(unix)]
fn sed_pipe(input: &str, program: &str) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("sed")
        .args(["-n", program])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sed spawns");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(input.as_bytes())
        .expect("snapshot written");
    let out = child.wait_with_output().expect("sed runs");
    assert!(out.status.success(), "sed exited non-zero");
    String::from_utf8(out.stdout).expect("utf8")
}

/// Writes an executable shim named `name` under `dir`, the same shape the
/// report tests use: a probe against a slow herdr must hit the timeout arm.
#[cfg(unix)]
fn write_shim(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("shim written");
    let mut perms = std::fs::metadata(&path)
        .expect("shim metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("shim chmod");
    path
}

/// RAII current-directory pin: sets the process cwd for the block and restores
/// it on drop, even on panic. Only `install`'s checkout branch reads cwd, and
/// no other test in this module observes it, so the pin is scoped to the one
/// test that needs it. nextest also runs each test in its own process.
#[cfg(unix)]
struct CwdPin {
    prev: PathBuf,
}

#[cfg(unix)]
impl CwdPin {
    fn new(dir: &Path) -> Self {
        let prev = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(dir).expect("set cwd");
        Self { prev }
    }
}

#[cfg(unix)]
impl Drop for CwdPin {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
    }
}

/// The probe bound: a herdr that never answers costs the caller the timeout,
/// never a hang. A 10 s sleep shim must resolve as no version inside roughly
/// the 2 s bound.
#[cfg(unix)]
#[test]
fn a_hung_herdr_bounds_the_probe_at_the_timeout() {
    let home = crate::testutil::HomeSandbox::new();
    let shim = write_shim(home.home(), "slow-herdr", "exec sleep 10");
    let start = std::time::Instant::now();
    let version = version_command(shim.to_str().expect("utf8 path"));
    assert!(version.is_none(), "a hung herdr resolves as no version");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "the probe is bounded, not the shim's full sleep"
    );
}

/// A failing `api snapshot` must not abort the open under `set -e`: the
/// snapshot's failure falls through to the plain call, and the shim's log
/// proves the open attempt ran (the abort path exits before any attempt).
#[cfg(unix)]
#[test]
fn a_failing_snapshot_still_attempts_the_open() {
    let home = crate::testutil::HomeSandbox::new();
    let herdr_shim = write_shim(
        home.home(),
        "herdr",
        "echo \"$@\" >> \"$(dirname \"$0\")/open.log\"; exit 1",
    );
    write_shim(home.home(), "clauth", "echo fit");
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/herdr-plugin/open-pane.sh");
    let out = std::process::Command::new("sh")
        .arg(path)
        .arg("tui")
        .env("HERDR_BIN_PATH", &herdr_shim)
        .env("HERDR_PLUGIN_ID", "clauth")
        .env(
            "PATH",
            format!(
                "{}:{}",
                home.home().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("script runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "the open fails with the shim's exit, not the snapshot's abort"
    );
    let log = std::fs::read_to_string(home.home().join("open.log")).unwrap_or_default();
    assert!(
        log.contains("--entrypoint tui"),
        "the open attempt ran after the failed snapshot: {log}"
    );
}

/// The fit branch's snapshot parse, pinned against the REAL 0.8.2 shape
/// (captured 2026-08-26): layout rows put `pane_id` right before `rect` and
/// spell the rect `{"height":H,"width":W,...}`; pane records carry no rect at
/// all, and they serialize AFTER the layouts, so the width pattern has to
/// backtrack past the pane record to the layout row. The two sed programs are
/// extracted from `open-pane.sh`'s source, so the pin reds when the script's
/// pattern drifts from the shape instead of leaving a second spelling that
/// can disagree with the script.
#[cfg(unix)]
#[test]
fn the_fit_sed_reads_the_real_snapshot_shape() {
    let script = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/herdr-plugin/open-pane.sh"
    ))
    .expect("open-pane.sh reads");

    // The focused-id line: a single-quoted program, verbatim between the
    // quotes.
    let focused_line = script
        .lines()
        .find(|l| l.contains("focused_pane_id") && l.contains("sed -n"))
        .expect("the focused-id sed line exists");
    let focused_prog = focused_line
        .split_once("sed -n '")
        .expect("single-quoted program")
        .1
        .split('\'')
        .next()
        .expect("closing quote");
    // The width line: a double-quoted program interpolating $focused, so the
    // extracted span gets the shell's double-quote processing (`\"` -> `"`)
    // and the id substituted, exactly as the script would run it.
    let width_line = script
        .lines()
        .find(|l| l.contains(r#"pane_id\":\"$focused"#))
        .expect("the width sed line exists");
    let width_span = width_line
        .split_once("sed -n \"")
        .expect("double-quoted program")
        .1
        .rsplit_once('"')
        .expect("closing quote")
        .0;

    let snap = concat!(
        r#"{"focused_pane_id":"wP:p68","layouts":[{"focused":true,"pane_id":"wP:p68","#,
        r#""rect":{"height":57,"width":206,"x":31,"y":1}}],"panes":[{"focused":true,"#,
        r#""foreground_cwd":"/x","pane_id":"wP:p68","revision":8}]}"#,
    );
    let focused = sed_pipe(snap, focused_prog);
    assert_eq!(focused.trim(), "wP:p68", "the focused id resolves");

    let width_prog = width_span
        .replace("\\\"", "\"")
        .replace("$focused", focused.trim());
    let width = sed_pipe(snap, &width_prog);
    assert_eq!(
        width.trim(),
        "206",
        "the width comes from the focused pane's layout row, not the pane record"
    );
}

/// Runs the real `herdr-plugin/open-pane.sh` with `herdr_body` as the herdr
/// shim (every call logs its argv as one line of `open.log`), the `clauth`
/// shim answering `knob` for `herdr config get popup_width`, and returns
/// (exit code, log).
#[cfg(unix)]
fn run_open_pane(home: &Path, herdr_body: &str, knob: &str) -> (Option<i32>, String) {
    let herdr_shim = write_shim(home, "herdr", herdr_body);
    write_shim(home, "clauth", &format!("echo {knob}"));
    let out = std::process::Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/herdr-plugin/open-pane.sh"
        ))
        .arg("tui")
        .env("HERDR_BIN_PATH", &herdr_shim)
        .env("HERDR_PLUGIN_ID", "clauth")
        .env(
            "PATH",
            format!(
                "{}:{}",
                home.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("script runs");
    let log = std::fs::read_to_string(home.join("open.log")).unwrap_or_default();
    (out.status.code(), log)
}

/// The `split-right` knob opens a real pane right of the focused one: the
/// open argv carries the placement/direction pair and no sizing flags, a
/// failed open is a failure (no plain-pair retry — that would open the
/// entrypoint's manifest placement, a popup, silently degrading the
/// requested split), and the split arm never reads the snapshot.
#[cfg(unix)]
#[test]
fn the_split_right_knob_opens_a_split_without_sizing_flags_or_a_plain_retry() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        "echo \"$@\" >> \"$(dirname \"$0\")/open.log\"; exit 1",
        "split-right",
    );
    assert_eq!(
        code,
        Some(1),
        "the shim's failure reaches the script's exit"
    );
    assert_eq!(
        log.matches("plugin pane open").count(),
        1,
        "a split arm never retries the plain pair: {log}"
    );
    assert!(
        !log.contains("api snapshot"),
        "a split arm skips the snapshot read: {log}"
    );
    let open_line = log
        .lines()
        .find(|l| l.contains("plugin pane open"))
        .expect("the open attempt ran: {log}");
    assert!(
        open_line.contains("--placement split --direction right"),
        "the split-right argv: {open_line}"
    );
    for sizing in ["--width", "--height", "--target-pane"] {
        assert!(
            !open_line.contains(sizing),
            "splits size by the pane grid, no {sizing}: {open_line}"
        );
    }
}

/// A split open has no singleton: "popup already open" is a popup-only
/// answer, so a split arm must not take it as success (the popup arms do —
/// the sibling test pins that half).
#[cfg(unix)]
#[test]
fn a_split_open_does_not_take_popup_already_open_as_success() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        "echo \"$@\" >> \"$(dirname \"$0\")/open.log\"; echo 'popup already open' >&2; exit 1",
        "split-right",
    );
    assert_eq!(
        code,
        Some(1),
        "the split arm does not swallow the popup answer"
    );
    assert_eq!(
        log.matches("plugin pane open").count(),
        1,
        "and it still never retries the plain pair: {log}"
    );
}

/// The popup arms keep the singleton dance: "popup already open" is the same
/// key pressed twice, exit 0.
#[cfg(unix)]
#[test]
fn a_popup_arm_takes_popup_already_open_as_success() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        "echo \"$@\" >> \"$(dirname \"$0\")/open.log\"; echo 'popup already open' >&2; exit 1",
        "half",
    );
    assert_eq!(
        code,
        Some(0),
        "the singleton answer is success for a popup arm"
    );
    assert!(
        log.contains("--entrypoint tui"),
        "the open attempt ran: {log}"
    );
}

/// The popup arms keep the old-herdr retry: a herdr refusing the sizing
/// flags gets the plain pair as a second attempt — still a popup, the
/// entrypoint's manifest placement — and its success decides the exit.
#[cfg(unix)]
#[test]
fn a_popup_arm_retries_the_plain_pair_when_the_sizing_flags_are_refused() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        concat!(
            "echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            // The shim's argv includes the `plugin pane open` prefix, so the
            // sized call is 9 words and the plain pair is 7.
            "case \"$#\" in\n",
            "  9) echo 'unknown option: --height' >&2; exit 1 ;;\n",
            "  7) ;;\n",
            "esac\n",
        ),
        "half",
    );
    assert_eq!(code, Some(0), "the plain-pair retry succeeds");
    assert_eq!(
        log.matches("plugin pane open").count(),
        2,
        "one flagged attempt, one plain retry: {log}"
    );
    assert!(
        log.lines()
            .next()
            .is_some_and(|l| l.contains("--height 50%")),
        "the first attempt carries the sizing flag: {log}"
    );
    assert_eq!(
        log.lines().last(),
        Some("plugin pane open --plugin clauth --entrypoint tui"),
        "the retry is the plain pair alone: {log}"
    );
}

/// The `split-top` knob splits the pane ABOVE the focused one: the shim log
/// shows the neighbor lookup (`pane neighbor --direction up --pane
/// <focused>`) and the open argv targets the neighbor with a downward split,
/// so the new pane lands directly above the focused pane.
#[cfg(unix)]
#[test]
fn the_split_top_knob_splits_the_pane_above_the_focused_one() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        concat!(
            "case \"$1\" in\n",
            "  api) echo '{\"focused_pane_id\":\"wP:p68\"}'\n",
            "       echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            "       ;;\n",
            "  pane) echo '{\"result\":{\"neighbor\":{\"pane_id\":\"wP:p68\",\"direction\":\"up\",\"neighbor_pane_id\":\"wP:p70\"}}}'\n",
            "        echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            "        ;;\n",
            "  *)   echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            "       ;;\n",
            "esac\n",
        ),
        "split-top",
    );
    assert_eq!(code, Some(0), "the split-top open succeeds");
    assert!(log.contains("api snapshot"), "the snapshot read ran: {log}");
    assert!(
        log.contains("pane neighbor --direction up --pane wP:p68"),
        "the neighbor lookup names the focused pane and the up direction: {log}"
    );
    let open_line = log
        .lines()
        .find(|l| l.contains("plugin pane open"))
        .expect("the open attempt ran: {log}");
    assert!(
        open_line.contains("--target-pane wP:p70 --placement split --direction down"),
        "the open splits the pane above downward: {open_line}"
    );
}

/// No pane above the focused one (`neighbor_pane_id` absent from the
/// answer): split-top splits the focused pane downward instead — the new
/// pane lands below the focused pane, but the knob keeps its split (a popup
/// fallback would abandon the knob).
#[cfg(unix)]
#[test]
fn the_split_top_knob_without_a_neighbor_splits_the_focused_pane_down() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        concat!(
            "case \"$1\" in\n",
            "  api) echo '{\"focused_pane_id\":\"wP:p68\"}'\n",
            "       echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            "       ;;\n",
            "  pane) echo '{\"result\":{\"neighbor\":{\"pane_id\":\"wP:p68\",\"direction\":\"up\"}}}'\n",
            "        echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            "        ;;\n",
            "  *)   echo \"$@\" >> \"$(dirname \"$0\")/open.log\"\n",
            "       ;;\n",
            "esac\n",
        ),
        "split-top",
    );
    assert_eq!(code, Some(0), "the fallback still opens a split");
    let open_line = log
        .lines()
        .find(|l| l.contains("plugin pane open"))
        .expect("the open attempt ran: {log}");
    assert!(
        open_line.contains("--target-pane wP:p68 --placement split --direction down"),
        "no neighbor splits the focused pane downward: {open_line}"
    );
}

/// A failed snapshot on split-top skips the neighbor lookup and the target
/// flag: herdr then splits the active pane downward, the same
/// below-the-focused shape, so the knob keeps its split.
#[cfg(unix)]
#[test]
fn the_split_top_knob_without_a_snapshot_splits_down_without_a_target() {
    let home = crate::testutil::HomeSandbox::new();
    let (code, log) = run_open_pane(
        home.home(),
        "echo \"$@\" >> \"$(dirname \"$0\")/open.log\"; exit 1",
        "split-top",
    );
    assert_eq!(
        code,
        Some(1),
        "the shim's failure reaches the script's exit"
    );
    assert!(
        !log.contains("pane neighbor"),
        "no neighbor lookup without a focused pane id: {log}"
    );
    let open_line = log
        .lines()
        .find(|l| l.contains("plugin pane open"))
        .expect("the open attempt ran: {log}");
    assert!(
        open_line.contains("--placement split --direction down"),
        "the fallback open keeps the downward split: {open_line}"
    );
    assert!(
        !open_line.contains("--target-pane"),
        "no target flag without a focused pane id: {open_line}"
    );
}

/// A hand-owned claude row is never rewritten: the strip removes only marked
/// blocks, so the hand-written row survives the resync and the plan hands the
/// knob-aware line over in a note.
#[test]
fn a_hand_owned_claude_row_survives_the_resync() {
    let existing = concat!(
        "# my config\n",
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["state_icon"], ["agent"]]"#,
        "\n"
    );
    let (text, plan, _) = resync_text(existing, "prefix+a", true).expect("resync");
    assert!(
        !plan.append.contains("rows_by_agent"),
        "never appended beside a hand-owned table"
    );
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("already sets a claude row")),
        "the hand-owned row keeps its verdict"
    );
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains(r#"["agent", "$clauth", "$clauth_delegate"]"#)),
        "the hand-merge line the note suggests carries the knob's token"
    );
    assert_eq!(
        text.matches("rows_by_agent").count(),
        1,
        "the user's own row is the only one: {text}"
    );
    assert!(
        text.contains(r#"claude = [["state_icon"], ["agent"]]"#),
        "the user's row content survives byte for byte: {text}"
    );
}

// ── `[herdr]` knob store + `clauth herdr config get` read path ─────────────

/// Seed a real-shaped profiles.toml through the resolver the app reads
/// (`clauth_dir()`), never a hand-built path, so the fixture pins the parse
/// path `load_config` walks.
fn write_profiles_toml(body: &str) {
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("profiles.toml"), body).expect("write profiles.toml");
}

#[test]
fn herdr_settings_round_trip_through_the_app_load_path() {
    let _home = crate::testutil::HomeSandbox::new();

    // No `[herdr]` table at all: the existing-file shape loads as defaults.
    write_profiles_toml("active_profile = \"acct\"\nprofiles = [\"acct\"]\n");
    let config = crate::profile::load_config().expect("load");
    assert_eq!(config.state.herdr, HerdrSettings::default());

    // A partial table fills the missing knobs from the documented defaults,
    // and the retired `full` popup width loads as Fit — the owner's merge
    // (full and fit resolved identically below the 540-col cap) — so a
    // profiles.toml written before the merge still loads.
    write_profiles_toml(concat!(
        "active_profile = \"acct\"\n",
        "profiles = [\"acct\"]\n",
        "[herdr]\n",
        "popup_width = \"full\"\n",
    ));
    let config = crate::profile::load_config().expect("load");
    assert_eq!(
        config.state.herdr,
        HerdrSettings {
            popup_width: PopupWidth::Fit,
            ..HerdrSettings::default()
        }
    );

    // A full table loads every knob, and a save + reload is a true round trip.
    write_profiles_toml(concat!(
        "active_profile = \"acct\"\n",
        "profiles = [\"acct\"]\n",
        "[herdr]\n",
        "popup_width = \"half\"\n",
        "pane_tag = false\n",
        "tag_watch_secs = 30\n",
        "border_label = true\n",
        "delegate_dot = false\n",
        "delegate_row_text = true\n",
    ));
    let config = crate::profile::load_config().expect("load");
    let want = HerdrSettings {
        popup_width: PopupWidth::Half,
        pane_tag: false,
        tag_watch_secs: 30,
        border_label: true,
        delegate_dot: false,
        delegate_row_text: true,
    };
    assert_eq!(config.state.herdr, want);
    crate::profile::save_app_state(&config.state).expect("save");
    let again = crate::profile::load_config().expect("reload");
    assert_eq!(again.state.herdr, want);
}

#[test]
fn saving_default_knobs_omits_the_herdr_block_until_one_moves() {
    let _home = crate::testutil::HomeSandbox::new();
    write_profiles_toml("active_profile = \"acct\"\nprofiles = [\"acct\"]\n");
    let config = crate::profile::load_config().expect("load");

    crate::profile::save_app_state(&config.state).expect("save");
    let text = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .expect("read");
    assert!(
        !text.contains("[herdr]"),
        "an untouched knob set must not grow a [herdr] block on save: {text}"
    );

    // One knob off its default is enough to persist the whole table.
    let mut moved = config.state.clone();
    moved.herdr.pane_tag = false;
    crate::profile::save_app_state(&moved).expect("save");
    let text = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .expect("read");
    assert!(
        text.contains("[herdr]"),
        "a moved knob must persist: {text}"
    );
    assert!(
        text.contains("pane_tag = false"),
        "knob value persists: {text}"
    );
}

#[test]
fn herdr_config_get_answers_every_default_with_no_profiles_toml() {
    let _home = crate::testutil::HomeSandbox::new();
    // Nothing written: the get path must answer the defaults, not an error.
    let config = crate::profile::load_config().expect("load");
    for (key, want) in [
        ("popup_width", "fit"),
        ("pane_tag", "on"),
        ("tag_watch_secs", "5"),
        ("border_label", "off"),
        ("delegate_dot", "on"),
        ("delegate_row_text", "off"),
    ] {
        assert_eq!(
            herdr_value(&config.state.herdr, key).expect("value"),
            want,
            "default for {key}"
        );
    }
}

#[test]
fn herdr_config_get_honors_written_knobs_through_the_real_load_path() {
    let _home = crate::testutil::HomeSandbox::new();
    write_profiles_toml(concat!(
        "active_profile = \"acct\"\n",
        "profiles = [\"acct\"]\n",
        "[herdr]\n",
        "popup_width = \"half\"\n",
        "pane_tag = false\n",
        "tag_watch_secs = 30\n",
        "border_label = true\n",
        "delegate_dot = false\n",
        "delegate_row_text = true\n",
    ));
    let config = crate::profile::load_config().expect("load");
    for (key, want) in [
        ("popup_width", "half"),
        ("pane_tag", "off"),
        ("tag_watch_secs", "30"),
        ("border_label", "on"),
        ("delegate_dot", "off"),
        ("delegate_row_text", "on"),
    ] {
        assert_eq!(
            herdr_value(&config.state.herdr, key).expect("value"),
            want,
            "written value for {key}"
        );
    }
}

#[test]
fn popup_width_round_trips_all_four_spellings_through_the_real_load_path() {
    let _home = crate::testutil::HomeSandbox::new();
    for spelling in ["fit", "half", "split-right", "split-top"] {
        write_profiles_toml(&format!(
            "active_profile = \"acct\"\nprofiles = [\"acct\"]\n\
             [herdr]\npopup_width = \"{spelling}\"\n"
        ));
        let config = crate::profile::load_config().expect("load");
        assert_eq!(
            herdr_value(&config.state.herdr, "popup_width").expect("value"),
            spelling,
            "the get path answers the written spelling"
        );
        crate::profile::save_app_state(&config.state).expect("save");
        let again = crate::profile::load_config().expect("reload");
        assert_eq!(
            again.state.herdr.popup_width, config.state.herdr.popup_width,
            "the {spelling} value survives a save + reload"
        );
        assert_eq!(
            herdr_value(&again.state.herdr, "popup_width").expect("value"),
            spelling,
            "the reloaded get path still answers {spelling}"
        );
    }
}

#[test]
fn herdr_config_get_unknown_key_is_a_usage_error_naming_the_valid_keys() {
    let _home = crate::testutil::HomeSandbox::new();
    let err = config_get("popup-width").expect_err("an unknown key must fail");
    let msg = err.to_string();
    for key in [
        "popup_width",
        "pane_tag",
        "tag_watch_secs",
        "border_label",
        "delegate_dot",
        "delegate_row_text",
    ] {
        assert!(msg.contains(key), "the error must name {key}: {msg}");
    }
    assert_eq!(crate::exit_code(Err(err)), 2, "a bad key is a usage error");
}

#[test]
fn herdr_config_get_parses_and_stays_out_of_herdrs_help() {
    let Cli {
        command:
            Some(Command::Herdr {
                cmd:
                    HerdrCommand::Config {
                        cmd: HerdrConfigCommand::Get { key },
                    },
            }),
        ..
    } = Cli::try_parse_from(["clauth", "herdr", "config", "get", "popup_width"])
        .expect("`herdr config get` must parse")
    else {
        panic!("`herdr config get` must select the get arm");
    };
    assert_eq!(key, "popup_width");

    // A bare `config` names no operation, and `get` takes exactly one key.
    assert!(Cli::try_parse_from(["clauth", "herdr", "config"]).is_err());
    assert!(Cli::try_parse_from(["clauth", "herdr", "config", "get"]).is_err());

    // Hidden-ish: parseable, completable, but absent from herdr's help.
    let help = Cli::command()
        .find_subcommand_mut("herdr")
        .expect("herdr subcommand")
        .render_long_help()
        .to_string();
    assert!(
        !help.contains("Print one herdr knob"),
        "the scripts' read path must stay out of the help surface"
    );
}

// ── report-profile.sh knob-off overrides, driven through the real script ────

/// Runs the real `herdr-plugin/report-profile.sh` against a shimmed `herdr`
/// and `clauth`: the herdr shim logs every `pane report-metadata` argv as one
/// line (and fails `process-info`, so a leaked watcher ends itself after its
/// retry budget instead of looping), the clauth shim answers `which` with
/// `fit` and dispatches `herdr config get <key>` to the knob values baked in
/// at write time. HOME, the state dir and the pane id all point into the
/// sandbox, so no live pane or real tree is touched. Returns the logged
/// report lines and whether the watcher pidfile appeared under the state
/// dir. `agent_json` is the event hook's `agent` field; `None` reads as no
/// claude agent, which the script leaves watcher-less.
///
/// The watcher verdict is captured HERE, not by the caller: the sandbox is
/// dropped when this returns, so a path-based check outside would always
/// read absent. The pidfile is created synchronously before the detached
/// spawn, so it exists here exactly when the script's watcher path ran.
#[cfg(unix)]
fn report_profile_run(
    pane_tag: &str,
    border_label: &str,
    agent_json: Option<&str>,
) -> (Vec<String>, bool) {
    let home = crate::testutil::HomeSandbox::new();
    write_shim(
        home.home(),
        "herdr",
        "if [ \"$1\" = pane ] && [ \"$2\" = report-metadata ]; then echo \"$*\" >> \"$(dirname \"$0\")/report.log\"; exit 0; fi\nif [ \"$1\" = pane ] && [ \"$2\" = process-info ]; then exit 1; fi\nexit 0\n",
    );
    write_shim(
        home.home(),
        "clauth",
        &format!(
            "case \"$1:$4\" in\n  which:) echo fit ;;\n  herdr:pane_tag) echo {pane_tag} ;;\n  herdr:border_label) echo {border_label} ;;\n  herdr:tag_watch_secs) echo 1 ;;\nesac\nexit 0\n"
        ),
    );
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/herdr-plugin/report-profile.sh"
    ))
    .env("HERDR_BIN_PATH", home.home().join("herdr"))
    .env("HERDR_PLUGIN_ID", "clauth")
    .env("HERDR_PANE_ID", "p1")
    .env("HERDR_PLUGIN_STATE_DIR", home.home().join("state"))
    .env("HOME", home.home())
    .env(
        "PATH",
        format!(
            "{}:{}",
            home.home().display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    if let Some(agent) = agent_json {
        cmd.env("HERDR_PLUGIN_EVENT_JSON", agent);
    }
    let out = cmd.output().expect("report-profile.sh runs");
    assert!(
        out.status.success(),
        "the script exits 0: stderr {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "fit\n",
        "the resolve prints the profile and nothing else"
    );
    let lines: Vec<String> = std::fs::read_to_string(home.home().join("report.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    for line in &lines {
        assert!(
            line.starts_with("pane report-metadata p1 --source "),
            "the pane id stays the first positional: {line}"
        );
    }
    let pidfile = home.home().join("state/watch-p1.pid");
    (lines, pidfile.exists())
}

/// `pane_tag` off publishes the token clear instead of silently skipping the
/// report, and still skips the watcher: one report-metadata call carrying
/// `--clear-token clauth` and no `--token`, and no pidfile under the state
/// dir. The agent reads as claude, so a watcher WOULD spawn if the off path
/// leaked past the gate.
#[cfg(unix)]
#[test]
fn pane_tag_off_publishes_the_token_clear_and_spawns_no_watcher() {
    let (lines, watcher_spawned) = report_profile_run("off", "on", Some(r#"{"agent":"claude"}"#));
    assert_eq!(
        lines.len(),
        1,
        "exactly one report-metadata call: {lines:?}"
    );
    let line = &lines[0];
    assert!(
        line.contains("--clear-token clauth"),
        "the token clear is published: {line}"
    );
    assert!(
        !line.contains("--token"),
        "no token publish beside the clear: {line}"
    );
    assert!(
        !watcher_spawned,
        "pane_tag off still skips the watcher spawn (no pidfile under the state dir)"
    );
}

/// `border_label` off publishes the display-agent clear instead of omitting
/// the artifact: the one-shot argv carries `--clear-display-agent` and no
/// `--display-agent`, while the on pane_tag still publishes the token.
#[cfg(unix)]
#[test]
fn border_label_off_publishes_the_display_agent_clear() {
    let (lines, _watcher_spawned) = report_profile_run("on", "off", None);
    assert_eq!(
        lines.len(),
        1,
        "exactly one report-metadata call: {lines:?}"
    );
    let line = &lines[0];
    assert!(
        line.contains("--clear-display-agent"),
        "the display-agent clear is published: {line}"
    );
    assert!(
        !line.contains("--display-agent"),
        "no display-agent publish beside the clear: {line}"
    );
    assert!(
        line.contains("--token clauth=fit"),
        "the on pane_tag still publishes the token: {line}"
    );
}

/// Regression control: both knobs on publishes today's artifacts unchanged —
/// `--token clauth=<profile>` and `--display-agent <profile>`, no clear flags.
#[cfg(unix)]
#[test]
fn both_knobs_on_publish_the_token_and_display_agent_unchanged() {
    let (lines, _watcher_spawned) = report_profile_run("on", "on", None);
    assert_eq!(
        lines.len(),
        1,
        "exactly one report-metadata call: {lines:?}"
    );
    let line = &lines[0];
    assert!(
        line.contains("--token clauth=fit"),
        "the token publish stays: {line}"
    );
    assert!(
        line.contains("--display-agent fit"),
        "the display-agent publish stays: {line}"
    );
    assert!(
        !line.contains("--clear-token"),
        "no clears on the on path: {line}"
    );
    assert!(
        !line.contains("--clear-display-agent"),
        "no clears on the on path: {line}"
    );
}

/// Both knobs off: herdr's one report-metadata call can carry both clears
/// (0.8.2 pane.rs refuses only set+clear of the SAME field), so the script
/// makes exactly one call and it publishes `--clear-token clauth` AND
/// `--clear-display-agent`.
#[cfg(unix)]
#[test]
fn both_knobs_off_publish_both_clears_in_one_call() {
    let (lines, _watcher_spawned) = report_profile_run("off", "off", None);
    assert_eq!(lines.len(), 1, "both clears ride the one call: {lines:?}");
    let line = &lines[0];
    assert!(
        line.contains("--clear-token clauth"),
        "the token clear is published: {line}"
    );
    assert!(
        line.contains("--clear-display-agent"),
        "the display-agent clear is published: {line}"
    );
    assert!(
        !line.contains("--token"),
        "no publishes on the off path: {line}"
    );
    assert!(
        !line.contains("--display-agent"),
        "no publishes on the off path: {line}"
    );
}

// ── report-profile.sh pane resolution, driven through the real script ────────

/// Runs the real `herdr-plugin/report-profile.sh` against shimmed `herdr`,
/// `ps` and `clauth`. The herdr shim answers `pane process-info` ONCE (its
/// second caller is the detached watcher, which must fail it three times so it
/// ends itself) and logs every `pane report-metadata` argv; the ps shim
/// answers ppid/args from the caller's scripted map; the clauth shim answers
/// `which` with `fit` and the config knobs. Rows land in the sandbox's
/// `~/.clauth/live_sessions`; `stale_row` names one whose mtime is pushed an
/// hour back, posing the dead-session leftover a recycled pid would leave.
/// Returns the logged report lines.
#[cfg(unix)]
fn report_profile_resolve_run(
    info_json: &str,
    ps_body: &str,
    rows: &[(&str, &str, u32)],
    stale_row: Option<&str>,
) -> Vec<String> {
    let home = crate::testutil::HomeSandbox::new();
    let sessions = home.home().join(".clauth/live_sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    for (sid, start, pid) in rows {
        let path = sessions.join(format!("{sid}.json"));
        let body = format!(
            r#"{{"session_id":"{sid}","start_profile":"{start}","pid":{pid},"started_at":0,"isolated":false,"follows_chain":false,"intended_member":null,"chain_cursor":null,"current_member":null,"last_swap_at":null}}"#
        );
        std::fs::write(&path, body).expect("row written");
        if stale_row == Some(sid) {
            crate::testutil::set_mtime(
                &path,
                std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
            );
        }
    }
    write_shim(
        home.home(),
        "herdr",
        &format!(
            "if [ \"$1\" = pane ] && [ \"$2\" = report-metadata ]; then echo \"$*\" >> \"$(dirname \"$0\")/report.log\"; exit 0; fi\nif [ \"$1\" = pane ] && [ \"$2\" = process-info ]; then if [ -f \"$(dirname \"$0\")/answered\" ]; then exit 1; fi; touch \"$(dirname \"$0\")/answered\"; printf '%s\\n' '{info_json}'; exit 0; fi\nexit 0\n"
        ),
    );
    write_shim(home.home(), "ps", ps_body);
    write_shim(
        home.home(),
        "clauth",
        "case \"$1:$4\" in\n  which:) echo fit ;;\n  herdr:pane_tag) echo on ;;\n  herdr:border_label) echo off ;;\n  herdr:tag_watch_secs) echo 1 ;;\nesac\nexit 0\n",
    );
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/herdr-plugin/report-profile.sh"
    ))
    .env("HERDR_BIN_PATH", home.home().join("herdr"))
    .env("HERDR_PLUGIN_ID", "clauth")
    .env("HERDR_PANE_ID", "p1")
    .env("HERDR_PLUGIN_EVENT_JSON", r#"{"agent":"claude"}"#)
    .env("HERDR_PLUGIN_STATE_DIR", home.home().join("state"))
    .env("HOME", home.home())
    .env(
        "PATH",
        format!(
            "{}:{}",
            home.home().display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    let out = cmd.output().expect("report-profile.sh runs");
    assert!(
        out.status.success(),
        "the script exits 0: stderr {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(home.home().join("report.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The token the resolve published; the four resolution tests below pin the
/// profile herdr would show on the pane, so the token value IS the verdict.
#[cfg(unix)]
fn token_line(lines: &[String]) -> String {
    assert_eq!(
        lines.len(),
        1,
        "exactly one report-metadata call: {lines:?}"
    );
    lines[0].clone()
}

/// The pane's own session resolves FIRST, off `foreground_process_group_id`.
/// The sweep order puts the delegate's claude before the supervisor, and the
/// delegate's row is keyed on the pane's `clauth mcp`, so a sweep-first
/// resolution names the delegate's account (D3) for a pane running uwuclxdy.
#[cfg(unix)]
#[test]
fn the_foreground_chain_beats_a_delegate_row_in_the_sweep() {
    let info = r#"{"process_info":{"foreground_process_group_id":1000,"foreground_processes":[{"pid":1001,"ppid":1002,"command":"claude"},{"pid":1002,"ppid":1000,"command":"clauth"},{"pid":1000,"ppid":1,"command":"clauth"}]}}"#;
    let ps = "case \"$*\" in\n  *'-o ppid='*) case \"$*\" in *' 1000') echo 1;; *' 1001') echo 1002;; *' 1002') echo 1000;; esac;;\n  *'-o args='*) case \"$*\" in *' 1002') echo 'clauth mcp';; *) echo other;; esac;;\nesac\nexit 0\n";
    let lines = report_profile_resolve_run(
        info,
        ps,
        &[("1000-0", "uwuclxdy", 1000), ("1002-0", "D3", 1002)],
        None,
    );
    assert!(
        token_line(&lines).contains("--token clauth=uwuclxdy"),
        "the pane's own session wins over the delegate row: {}",
        lines[0]
    );
}

/// A bare `claude` pane (foreground present, no registered session) answers
/// the global account, never the account of a delegate it happens to host:
/// the sweep must not run when the foreground chain resolved cleanly to no row.
#[cfg(unix)]
#[test]
fn a_bare_pane_hosting_a_delegate_still_answers_the_global_account() {
    let info = r#"{"process_info":{"foreground_process_group_id":1001,"foreground_processes":[{"pid":1002,"ppid":1003,"command":"claude"},{"pid":1003,"ppid":1001,"command":"clauth"},{"pid":1001,"ppid":1,"command":"claude"}]}}"#;
    let ps = "case \"$*\" in\n  *'-o ppid='*) case \"$*\" in *' 1002') echo 1003;; *' 1003') echo 1001;; *' 1001') echo 1;; esac;;\n  *'-o args='*) case \"$*\" in *' 1003') echo 'clauth mcp';; *) echo other;; esac;;\nesac\nexit 0\n";
    let lines = report_profile_resolve_run(info, ps, &[("1003-0", "D3", 1003)], None);
    assert!(
        token_line(&lines).contains("--token clauth=fit"),
        "the bare pane answers the global account: {}",
        lines[0]
    );
}

/// The compat sweep (a process-info without the foreground field) skips a
/// delegate child by its parent and refuses to match a row on an mcp hop, so
/// it climbs through the mcp to the pane's own supervisor instead of stopping
/// at the delegate's row.
#[cfg(unix)]
#[test]
fn the_compat_sweep_climbs_through_an_mcp_without_matching_its_rows() {
    let info = r#"{"process_info":{"foreground_processes":[{"pid":1002,"ppid":1003,"command":"claude"},{"pid":1003,"ppid":1001,"command":"clauth"},{"pid":1001,"ppid":1000,"command":"claude"},{"pid":1000,"ppid":1,"command":"clauth"}]}}"#;
    let ps = "case \"$*\" in\n  *'-o ppid='*) case \"$*\" in *' 1002') echo 1003;; *' 1003') echo 1001;; *' 1001') echo 1000;; *' 1000') echo 1;; esac;;\n  *'-o args='*) case \"$*\" in *' 1003') echo 'clauth mcp';; *) echo other;; esac;;\nesac\nexit 0\n";
    let lines = report_profile_resolve_run(
        info,
        ps,
        &[("1000-0", "uwuclxdy", 1000), ("1003-0", "D3", 1003)],
        None,
    );
    assert!(
        token_line(&lines).contains("--token clauth=uwuclxdy"),
        "the sweep climbs past the mcp to the pane's supervisor: {}",
        lines[0]
    );
}

/// Two rows carrying one pid — the D2/DS4 aliasing measured live 2026-09-03,
/// a finished delegate's stale row beside a newer one — resolve to the
/// NEWEST, never the alphabetically-first file. The ids sort so the stale one
/// is alphabetically first: the old `head -n 1` picks it, mtime picks the
/// live one.
#[cfg(unix)]
#[test]
fn two_rows_on_one_pid_resolve_to_the_newest() {
    let info = r#"{"process_info":{"foreground_process_group_id":1000,"foreground_processes":[{"pid":1000,"ppid":1,"command":"clauth"}]}}"#;
    let ps = "case \"$*\" in\n  *'-o ppid='*) case \"$*\" in *' 1000') echo 1;; esac;;\n  *'-o args='*) echo other;;\nesac\nexit 0\n";
    let lines = report_profile_resolve_run(
        info,
        ps,
        &[("1823355-1", "D2", 1000), ("1823355-3", "DS4", 1000)],
        Some("1823355-1"),
    );
    assert!(
        token_line(&lines).contains("--token clauth=DS4"),
        "the newest row wins over the stale one: {}",
        lines[0]
    );
}

/// The foreground-field parse, pinned against the REAL 0.8.2 process-info
/// shape (captured 2026-09-03): the sed program is extracted from
/// `report-profile.sh`'s source, so the pin reds when the script's pattern
/// drifts from the shape instead of leaving a second spelling that can
/// disagree with the script.
#[cfg(unix)]
#[test]
fn the_fg_sed_reads_the_real_process_info_shape() {
    let script = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/herdr-plugin/report-profile.sh"
    ))
    .expect("report-profile.sh reads");
    let fg_line = script
        .lines()
        .find(|l| l.contains("foreground_process_group_id") && l.contains("sed -n"))
        .expect("the fg sed line exists");
    let fg_prog = fg_line
        .split_once("sed -n '")
        .expect("single-quoted program")
        .1
        .split('\'')
        .next()
        .expect("program body");

    // The real 0.8.2 bytes: `process_info` carries the field before the
    // process array, compact JSON, one line.
    let real = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":1822495,"foreground_processes":[{"argv":["clauth","start","uwuclxdy","--effort","max","/handoff reusable: resume nyatrade queue. read @docs/handoff-state.md first, then the handoff skill's runner protocol from step 1."],"cmdline":"clauth start uwuclxdy --effort max /handoff reusable: resume nyatrade queue. read @docs/handoff-state.md first, then the handoff skill's runner protocol from step 1.","pid":1822495,"ppid":2719707,"cwd":"/home/uwuclxdy/repos/py/nyatrade"}]}}}"#;
    // The capture ends without a trailing newline, and the script consumes the
    // answer through command substitution, so the pin compares trimmed.
    assert_eq!(
        sed_pipe(real, fg_prog).trim(),
        "1822495",
        "the fg sed resolves the real shape"
    );
}
