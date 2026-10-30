//! Inline tests for `plugin_host`: the host wiring and its self-heal behind
//! the spawn-free pre-check. The registry shapes the pre-check reads are
//! pinned by agentgear's own unit tests, and the lifecycle itself (the real
//! `claude` CLI as transaction boundary) is pinned hermetically by the
//! fake-claude install test in `tui_app.rs` and exercised for real in the
//! scratch-profile verifies.

use super::ClauthPlugin;
use agentgear::PluginHost;

/// The derive and the one-line `build.rs` are the whole of the agentgear
/// wiring; if either silently broke (name drift, a version guard that stopped
/// pinning, an embed that stopped baking the tree), these fail without
/// spawning the binary.
#[test]
fn derive_metadata_is_wired() {
    assert_eq!(ClauthPlugin::NAME, "clauth");
    assert_eq!(ClauthPlugin::MARKETPLACE, "clauth");
    assert_eq!(ClauthPlugin::AGENTS, &["claude"]);
    // build.rs pins plugins/.claude-plugin/plugin.json `version` to this, so
    // the const equals the crate version.
    assert_eq!(ClauthPlugin::VERSION, env!("CARGO_PKG_VERSION"));

    let descriptor = ClauthPlugin::descriptor();
    assert_eq!(descriptor.name, "clauth");
    assert_eq!(descriptor.id(), "clauth@clauth");
    assert_eq!(descriptor.version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn embedded_tree_is_baked_in() {
    // The `embed` feature compresses `plugins/` into the binary; an empty blob
    // would mean `install(Scope::User, Source::Embedded)` errors at
    // materialize instead of installing.
    assert!(
        !ClauthPlugin::embedded_blob().is_empty(),
        "the plugin tree was not embedded"
    );
}

/// The delegate hook's wake-notification title: the bundled manifest names the
/// event in `rewakeSummary` instead of letting the host render its internal
/// "Stop hook feedback" placeholder, which reads as an empty notification. The
/// summary and message must cover completed jobs, pending jobs, and a mixture
/// of both. A pending-only wake has no result yet. These literals prevent the
/// notification from claiming completion when the body only contains a reminder.
#[test]
fn the_delegate_hook_names_its_wake_summary_instead_of_the_host_placeholder() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let (_, entry) = await_job_hook(&hooks);
    assert_eq!(
        entry["rewakeSummary"], "clauth delegate update",
        "the manifest names the wake notification instead of the host's placeholder"
    );
    assert_ne!(
        entry["rewakeSummary"], "Stop hook feedback",
        "the host's internal placeholder must never be the shipped summary"
    );
    assert_eq!(
        entry["rewakeMessage"], "A clauth delegate result or status update follows.",
        "the wake message must also describe a pending-only reminder"
    );
}

/// The `PostToolUse` group and hook entry running `clauth mcp-await-job`,
/// found by its command so a matcher edit cannot hide them from the tests
/// pinning them. Counted over every `PostToolUse` entry: a second copy there
/// would spawn the hook on calls no test covers.
fn await_job_hook(hooks: &serde_json::Value) -> (&serde_json::Value, &serde_json::Value) {
    let found: Vec<(&serde_json::Value, &serde_json::Value)> = hooks["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse is an array")
        .iter()
        .flat_map(|group| {
            group["hooks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |entry| (group, entry))
        })
        .filter(|(_, entry)| entry["command"] == "clauth mcp-await-job")
        .collect();
    assert_eq!(
        found.len(),
        1,
        "exactly one PostToolUse hook runs `clauth mcp-await-job`"
    );
    found[0]
}

/// A session reaches `delegate` under the plugin's server name, or under the
/// bare `clauth` server name when a user-scope `clauth` MCP server suppresses
/// the plugin's; a backgrounded result reaches a stopped session unprompted
/// only through this hook, so the matcher must take both names and nothing
/// else. The host tests this matcher as an unanchored JS regex, and the
/// literal is pinned by equality because the `regex` crate stands in for that
/// engine below: a respelling must first re-establish that both read it alike.
#[test]
fn the_await_job_hook_matches_delegate_under_both_server_names() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let matcher = await_job_hook(&hooks).0["matcher"]
        .as_str()
        .expect("the await-job group carries a matcher");
    assert_eq!(
        matcher, "^mcp__(plugin_clauth_)?clauth__delegate$",
        "respelling the await-job matcher: re-check that JS RegExp and the regex crate read it \
         alike, then update this literal"
    );
    let matcher = regex::Regex::new(matcher).expect("the matcher compiles as a regex");
    for tool in [
        "mcp__plugin_clauth_clauth__delegate",
        "mcp__clauth__delegate",
    ] {
        assert!(matcher.is_match(tool), "{tool:?} must wake on its result");
    }
    for tool in [
        "mcp__clauth__monitor",
        "mcp__plugin_clauth_clauth__monitor",
        "mcp__clauth__profiles",
        "mcp__clauth__switch_profile",
        "mcp__clauth__delegates",
        "mcp__notclauth__delegate",
        "mcp__plugin_other_clauth__delegate",
        "mcp__foo_mcp__clauth__delegate",
    ] {
        assert!(!matcher.is_match(tool), "{tool:?} must not run the hook");
    }
}

/// Every command a hooks.json event runs, across its matcher groups.
fn event_commands(hooks: &serde_json::Value, event: &str) -> Vec<String> {
    hooks["hooks"][event]
        .as_array()
        .unwrap_or_else(|| panic!("{event} is an array"))
        .iter()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .map(str::to_string)
        .collect()
}

/// The SessionStart wiring the self-heal rides on: the committed hooks.json
/// must carry BOTH hooks — the profile-change note keeps working, and the new
/// self-heal entry points at the hidden `clauth self-heal` subcommand. A drift
/// here (someone edits hooks.json and drops one command) silently disables a
/// session behavior, which is exactly what this test exists to catch.
#[test]
fn session_start_hook_wires_self_heal_beside_the_note() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let commands = event_commands(&hooks, "SessionStart");
    assert!(
        commands
            .iter()
            .any(|c| c == "clauth hook-profile-changed-note"),
        "the profile-change note must keep its SessionStart slot: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "clauth self-heal"),
        "the self-heal hook is not wired into SessionStart: {commands:?}"
    );
}

/// A prompt is the main scope's backstop for the note: a turn whose tool
/// calls all fall inside a cadence window the previous turn opened would
/// otherwise go unchecked. The UserPromptSubmit leg must keep its entry.
#[test]
fn prompt_submit_keeps_the_note_command() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let commands = event_commands(&hooks, "UserPromptSubmit");
    assert!(
        commands
            .iter()
            .any(|c| c == "clauth hook-profile-changed-note"),
        "the profile-change note must keep its UserPromptSubmit slot: {commands:?}"
    );
}

/// The mid-turn note is one process spawn per cadence window in the hooks
/// module. A `PostToolUse` entry whose matcher takes every tool spawns a
/// command on each call, so none may exist: an absent, empty, `*` or `.*`
/// matcher all run for any tool. Each matcher is read as a regex and checked
/// against ordinary tool names; one the `regex` crate cannot read fails here
/// rather than passing unseen. `Agent` is left out: a matcher naming only the
/// agent-spawn tools is a legitimate shape.
#[test]
fn no_post_tool_use_entry_runs_on_every_tool_call() {
    let hooks: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json"))
            .expect("plugins/hooks/hooks.json parses");
    let groups = hooks["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse is an array");
    for entry in groups {
        let matcher = entry
            .get("matcher")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(
            !matcher.is_empty(),
            "a PostToolUse entry without a matcher runs on every tool call: {entry}"
        );
        let matcher =
            regex::Regex::new(matcher).expect("a PostToolUse matcher compiles as a regex");
        for tool in ["Bash", "Read", "Write"] {
            assert!(
                !matcher.is_match(tool),
                "{tool:?} is an ordinary call, so the matcher runs on every tool call: {entry}"
            );
        }
    }
}

/// The note window map's sweep is its only bound on growth besides a
/// subagent's stop. `claude plugin test` loads the module in its own graph, so
/// a plugin test cannot read the map size (measured 2026-10-30: an exported
/// counter read 0 and a global read -1); the sweep is pinned at the
/// source instead. What that cannot see: whether the sweep runs at all, and
/// whether a `return`, an uncalled function or a `/* */` block comment inside
/// the handler span leaves it dead code.
#[test]
fn the_note_window_sweep_bounds_the_map() {
    let source = include_str!("../../plugins/hooks/register.ts");
    assert!(
        source.contains("for (const [stale, at] of lastNoteAt)"),
        "register.ts must keep the note window sweep: it is the map's only \
         bound besides a subagent's stop"
    );
    assert!(
        source.contains("lastNoteAt.delete(stale)"),
        "the sweep must drop the entry it found stale, not just walk the map"
    );
    let handler = source
        .find("on('classic.PostToolUse'")
        .expect("register.ts registers a classic.PostToolUse handler");
    let stamp = handler
        + source[handler..]
            .find("lastNoteAt.set(key, now)")
            .expect("the handler stamps the window before it runs the note");
    // Comment lines are dropped first: an out-commented sweep still carries
    // both substrings, so a raw `contains` reads it as present.
    let body = source[handler..stamp]
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        body.contains(
            "for (const [stale, at] of lastNoteAt) { if (now - at >= NOTE_CADENCE_MS) \
             lastNoteAt.delete(stale) }"
        ),
        "the window stamp must be preceded, inside classic.PostToolUse, by the \
         sweep loop and its bare NOTE_CADENCE_MS threshold: {body}"
    );
}

/// The hooks module skips the note cadence on a main-scope agent-spawn call
/// so the headroom nudge still fires there, from a list of tool names it
/// mirrors from clauth. Both sides must name the same tools: a name only one
/// side knows silently drops that spawn's nudge.
#[test]
fn module_and_clauth_name_the_same_agent_spawn_tools() {
    let source = include_str!("../../plugins/hooks/register.ts");
    let decl = source
        .find("const AGENT_SPAWN_TOOLS")
        .expect("register.ts names its agent-spawn tool list");
    // Bound the parse to the declaration's own line, so a respelling with no
    // `= [` reds here instead of parsing a later declaration's list.
    let line = source[decl..]
        .lines()
        .next()
        .expect("the declaration line is not empty");
    let open = line
        .find("= [")
        .expect("the list follows the declaration on its own line")
        + 3;
    let close = open + line[open..].find(']').expect("the list closes on its line");
    let module: Vec<&str> = line[open..close]
        .split(',')
        .map(|name| name.trim().trim_matches('\''))
        .filter(|name| !name.is_empty())
        .collect();
    assert!(!module.is_empty(), "the parsed list must not be empty");
    assert_eq!(
        module,
        crate::hook_note::AGENT_SPAWN_TOOLS.to_vec(),
        "register.ts's agent-spawn list drifted from clauth's predicate"
    );
}

/// The hook's output contract: a line appears only when the heal changed
/// something. Healthy means silent — the planted `if true` mutation that made
/// every heal print reds here — and a real transition (a stale marker cleared
/// after the registration vanished) prints exactly once, in the hook's own
/// wording. Runs hermetically through the fake-`claude` harness in
/// `testutil`.
#[cfg(unix)]
#[test]
fn self_heal_says_nothing_when_healthy_and_reports_changes() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);

    // A fresh install through the same host the TUI fix calls: registry entry
    // present, marker stamped.
    assert!(
        matches!(
            super::install().expect("install"),
            agentgear::Outcome::Installed
        ),
        "the fixture install must land"
    );

    // Healthy + marker present: nothing to say.
    assert_eq!(
        super::self_heal_line().expect("heal"),
        None,
        "a healthy install prints nothing"
    );

    // The registration's backing vanishes (registry entry + shim state), the
    // marker stays: the heal clears the stale marker and says so once.
    std::fs::remove_file(config.join("plugins").join("installed_plugins.json"))
        .expect("remove registry");
    std::fs::remove_file(std::env::var_os("CLAUDE_SHIM_STATE").expect("shim state pin"))
        .expect("remove shim state");
    assert_eq!(
        super::self_heal_line().expect("heal"),
        Some("clauth self-heal: cleared stale marker".to_string()),
        "a heal that changed something says so, in the hook's own wording"
    );

    // Marker cleared + nothing registered: silent again.
    assert_eq!(
        super::self_heal_line().expect("heal"),
        None,
        "after the clear there is nothing left to say"
    );
    let _ = &fake;
}

// ── the spawn-free pre-check gate ─────────────────────────────────────────

/// The hook's spawn-free contract: two hook runs in a row on a healthy install
/// spawn no `claude` at all. The install's own spawns are the baseline; the
/// hook runs must add nothing to the shim log.
#[cfg(unix)]
#[test]
fn self_heal_spawns_no_claude_on_a_healthy_registration() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);
    let baseline = fake.log();

    super::self_heal().expect("first hook run");
    super::self_heal().expect("second hook run");
    assert_eq!(
        fake.log(),
        baseline,
        "a healthy registration must spawn no claude from the hook"
    );
}

/// The other half of the spawn-free pin: a hook run on a registration the
/// pre-check flags still heals, so a hook that never heals at all cannot pass
/// the spawn-free pin above. A dead installPath is the shape: the entry
/// survives a per-session config dir's collection, its `installPath` dies, and
/// only the heal rewrites it.
#[cfg(unix)]
#[test]
fn self_heal_still_heals_a_broken_registration() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();

    super::self_heal().expect("hook run");
    let after = fake.log();
    let grown = after.strip_prefix(baseline.as_str()).unwrap_or_default();
    assert!(
        grown.contains("plugin list --json"),
        "a broken registration must reach the heal's `claude plugin` calls, got:\n{grown}"
    );
}

/// A re-point failure must not skip the heal: the installPath convergence leg
/// runs beside the heal, so a registry the re-point cannot read still reaches
/// the heal (the pre-check reads the unparseable registry as "cannot tell" and
/// heals conservatively), and the re-point error still surfaces from the hook.
#[cfg(unix)]
#[test]
fn self_heal_still_heals_past_a_repoint_error() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).expect("~/.claude");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&claude);

    // Both legs read the one config dir here: the re-point leg fails on the
    // non-UTF-8 bytes, and the pre-check reads them as "cannot tell" (heal).
    let installed_path = claude.join("plugins").join("installed_plugins.json");
    std::fs::write(&installed_path, [0xff, 0xfe, 0xfd]).expect("corrupt registry");
    let baseline = fake.log();

    let result = super::self_heal();
    let after = fake.log();
    let grown = after.strip_prefix(baseline.as_str()).unwrap_or_default();
    assert!(
        grown.contains("plugin list --json"),
        "the heal must still run past a re-point error, got:\n{grown}"
    );
    let err = result.expect_err("the re-point error still surfaces from the hook");
    assert!(
        format!("{err:#}").starts_with(&format!("reading {}: not utf-8", installed_path.display())),
        "the error is the re-point read, not a heal error, got: {err:#}"
    );
}

/// A box that never installed the plugin (no registry, no marker) spawns
/// nothing from the hook: the pre-check reads Clean, so the hook stays quiet.
#[cfg(unix)]
#[test]
fn self_heal_spawns_nothing_on_a_never_installed_box() {
    use crate::testutil::{FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let fake = FakeClaude::new(&home);

    super::self_heal().expect("hook run");
    super::self_heal().expect("second hook run");
    assert!(
        fake.log().is_empty(),
        "a never-installed box must spawn no claude from the hook, got:\n{}",
        fake.log()
    );
}

/// The stand-down must not fire when this process's own executable is unchanged
/// since it started: a flagged registration still heals. The seam pins the
/// recorded path, the file is left unchanged, and the decision stats it for
/// real — so the comparison reads "unchanged".
#[cfg(unix)]
#[test]
fn self_heal_heals_when_this_process_is_unchanged() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    // Pin the recorded path and leave the file in place, so the decision's
    // real stat reads "unchanged".
    let self_exe = crate::testutil::write_shim(home.home(), "self-exe", "exit 0");
    let _self = super::SelfExeOverride::pin(Some(self_exe.as_path()));

    // Break the registration so the gate fires, then the hook must heal.
    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();

    super::self_heal().expect("hook run");
    let after = fake.log();
    let grown = after.strip_prefix(baseline.as_str()).unwrap_or_default();
    assert!(
        grown.contains("plugin list --json"),
        "an unchanged self image must still heal a flagged registry, got:\n{grown}"
    );
}

/// The stand-down: when this process's own executable was replaced since it
/// started, the hook skips the heal (no `claude` spawn) and says so once per
/// process, naming the recorded path. "Replaced" is a real rename-replace of
/// the recorded file — a new file written beside it, then `rename` over it — so
/// the decision's real stat sees a new inode.
#[cfg(unix)]
#[test]
fn self_heal_stands_down_when_this_process_was_replaced() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    super::reset_stand_down_report_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    // Pin the recorded path, then replace the file at that path with a new
    // inode: write a new file beside it and `rename` it over.
    let recorded = crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0");
    let _self = super::SelfExeOverride::pin(Some(recorded.as_path()));
    let replacement =
        crate::testutil::write_shim(home.home(), "self-exe-replacement", "exit 0 # replaced");
    std::fs::rename(&replacement, &recorded).expect("rename-replace the recorded file");

    // Break the registration so the gate would fire; the stand-down must win.
    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    super::self_heal().expect("hook run");
    super::self_heal().expect("second hook run");

    assert_eq!(
        fake.log(),
        baseline,
        "a replaced self image must spawn no claude from the hook"
    );
    assert_eq!(
        lines.snapshot(),
        [format!(
            "clauth: skipping the plugin heal because this process is still running the \
             old binary ({})",
            recorded.display()
        )],
        "the stand-down is said once per process, naming the recorded path"
    );
}

/// Drive the hook once after `replace` changes the recorded file, and assert
/// it stood down: no `claude` spawn, and the one stand-down line naming the
/// recorded path. `replace` gets the recorded path and the sandbox home.
#[cfg(unix)]
fn assert_hook_stands_down_after(replace: impl FnOnce(&std::path::Path, &std::path::Path)) {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    super::reset_stand_down_report_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    let recorded = crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0");
    let _self = super::SelfExeOverride::pin(Some(recorded.as_path()));
    replace(&recorded, home.home());

    // Break the registration so the gate would fire; the stand-down must win.
    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    super::self_heal().expect("hook run");

    assert_eq!(
        fake.log(),
        baseline,
        "a replaced self image must spawn no claude from the hook"
    );
    assert_eq!(
        lines.snapshot(),
        [format!(
            "clauth: skipping the plugin heal because this process is still running the \
             old binary ({})",
            recorded.display()
        )]
    );
}

/// A same-size file carrying the recorded file's mtime, renamed over it, is
/// still a replacement: only the new inode tells it apart.
#[cfg(unix)]
#[test]
fn self_heal_stands_down_when_a_same_size_same_mtime_file_is_renamed_over_it() {
    assert_hook_stands_down_after(|recorded, home| {
        let mtime = std::fs::metadata(recorded)
            .and_then(|meta| meta.modified())
            .expect("recorded mtime");
        // Same length as the recorded "exit 0" body.
        let replacement = crate::testutil::write_shim(home, "self-exe-same-size", "exit 1");
        crate::testutil::set_mtime(&replacement, mtime);
        std::fs::rename(&replacement, recorded).expect("rename-replace the recorded file");
    });
}

/// An in-place rewrite (the same inode, as `cp new <installed>` does on macOS)
/// of the same length is a replacement: only the moved mtime tells it apart.
#[cfg(unix)]
#[test]
fn self_heal_stands_down_when_its_file_is_rewritten_in_place() {
    assert_hook_stands_down_after(|recorded, _home| {
        let mtime = std::fs::metadata(recorded)
            .and_then(|meta| meta.modified())
            .expect("recorded mtime");
        std::fs::write(recorded, "#!/bin/sh\nexit 1\n").expect("rewrite in place");
        crate::testutil::set_mtime(recorded, mtime + std::time::Duration::from_secs(60));
    });
}

/// An in-place rewrite of the same length inside the same second is still a
/// replacement: `cp` onto the installed binary right after an install moves
/// the mtime by less than a second. Needs a temp filesystem storing mtimes at
/// 1 ms or finer (ext4, APFS); on a whole-second one the rewrite is invisible
/// to the product too, and this test reds.
#[cfg(unix)]
#[test]
fn self_heal_stands_down_when_its_file_is_rewritten_in_place_within_the_second() {
    assert_hook_stands_down_after(|recorded, _home| {
        let mtime = std::fs::metadata(recorded)
            .and_then(|meta| meta.modified())
            .expect("recorded mtime");
        let since_epoch = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .expect("mtime after the epoch");
        // Land in the recorded mtime's own second but off its sub-second
        // part, so only a reading finer than whole seconds sees the move.
        let offset = if since_epoch.subsec_millis() == 1 {
            2
        } else {
            1
        };
        let same_second = std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(since_epoch.as_secs())
            + std::time::Duration::from_millis(offset);
        std::fs::write(recorded, "#!/bin/sh\nexit 1\n").expect("rewrite in place");
        crate::testutil::set_mtime(recorded, same_second);
    });
}

/// An in-place rewrite that keeps the recorded mtime is a replacement when its
/// length moved: only the length tells it apart.
#[cfg(unix)]
#[test]
fn self_heal_stands_down_when_its_file_grows_in_place_under_the_same_mtime() {
    assert_hook_stands_down_after(|recorded, _home| {
        let mtime = std::fs::metadata(recorded)
            .and_then(|meta| meta.modified())
            .expect("recorded mtime");
        std::fs::write(recorded, "#!/bin/sh\nexit 0 # grown\n").expect("rewrite in place");
        crate::testutil::set_mtime(recorded, mtime);
    });
}

/// The stand-down also fires when the recorded path is gone: the hook skips
/// the heal and says so, naming the recorded path. The decision's real stat of
/// a deleted file reads "gone".
#[cfg(unix)]
#[test]
fn self_heal_stands_down_when_this_process_is_gone() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    super::reset_stand_down_report_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    // Pin the recorded path, then delete the file at that path.
    let recorded = crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0");
    let _self = super::SelfExeOverride::pin(Some(recorded.as_path()));
    std::fs::remove_file(&recorded).expect("delete the recorded file");

    // Break the registration so the gate would fire; the stand-down must win.
    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    super::self_heal().expect("hook run");

    assert_eq!(
        fake.log(),
        baseline,
        "a gone self image must spawn no claude from the hook"
    );
    assert_eq!(
        lines.snapshot(),
        [format!(
            "clauth: skipping the plugin heal because this process's binary ({}) was \
             removed",
            recorded.display()
        )],
        "a gone self image stands down, naming the recorded path"
    );

    // A new binary lands at the path (a reinstall, or the rename-in that ends a
    // Windows self-update): the log names that change once too, and a second
    // round of either kind adds nothing.
    crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0 # reinstalled");
    super::self_heal().expect("hook run after the reinstall");
    std::fs::remove_file(&recorded).expect("delete it again");
    super::self_heal().expect("hook run after the second removal");
    assert_eq!(fake.log(), baseline, "still no claude spawn");
    assert_eq!(
        lines.snapshot(),
        [
            format!(
                "clauth: skipping the plugin heal because this process's binary ({}) was \
                 removed",
                recorded.display()
            ),
            format!(
                "clauth: skipping the plugin heal because this process is still running the \
                 old binary ({})",
                recorded.display()
            ),
        ],
        "each kind is said once per process"
    );
}

/// Recording this process's executable failed at start, so the stand-down
/// fails open and a flagged registration still heals, the fail-open note said
/// once per process.
#[cfg(unix)]
#[test]
fn self_heal_heals_when_the_recording_failed() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    super::reset_record_failure_report_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    // `None` recorded poses "recording failed at start".
    let _self = super::SelfExeOverride::pin(None);

    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    super::self_heal().expect("hook run");
    super::self_heal().expect("second hook run");
    let after = fake.log();
    let grown = after.strip_prefix(baseline.as_str()).unwrap_or_default();
    assert!(
        grown.contains("plugin list --json"),
        "a failed recording must still heal a flagged registry, got:\n{grown}"
    );
    assert_eq!(
        lines.snapshot(),
        ["clauth: could not find this process's binary at start"],
        "the fail-open note is said once"
    );
}

/// The production identity read, exercised explicitly through the seam: this
/// test binary's own image is unchanged, so the hook heals a flagged
/// registration through the real record-and-compare path. The mutant that
/// inverts the decision's replaced arm reds here. This test reads the real
/// process record by design, so it reds if the test binary is rebuilt in
/// place while it runs (a second `cargo` run in the same build dir); the
/// seam-pinned heal tests do not.
#[cfg(unix)]
#[test]
fn self_heal_heals_through_the_production_read_when_this_process_is_unchanged() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let _production = super::SelfExeOverride::production();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();

    super::self_heal().expect("hook run");
    let after = fake.log();
    let grown = after.strip_prefix(baseline.as_str()).unwrap_or_default();
    assert!(
        grown.contains("plugin list --json"),
        "the production read must see this test binary unchanged and heal, got:\n{grown}"
    );
}

/// The detached leg's copy of the stand-down: a replaced self image makes
/// `heal_detached` skip the heal even when the gate fires, and says so once,
/// naming the recorded path. "Replaced" is a real rename-replace of the
/// recorded file.
#[cfg(unix)]
#[test]
fn heal_detached_stands_down_when_this_process_was_replaced() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox, join_background_tasks};

    let home = HomeSandbox::new();
    super::reset_stand_down_report_for_test();
    super::reset_heal_throttle_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    let recorded = crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0");
    let _self = super::SelfExeOverride::pin(Some(recorded.as_path()));
    let replacement =
        crate::testutil::write_shim(home.home(), "self-exe-replacement", "exit 0 # replaced");
    std::fs::rename(&replacement, &recorded).expect("rename-replace the recorded file");

    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["installPath"] =
        serde_json::json!("/gone/runtime/plugins/cache");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed broken registry");
    let baseline = fake.log();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    super::heal_detached();
    join_background_tasks();
    assert_eq!(
        fake.log(),
        baseline,
        "a replaced self image must make the detached heal skip the spawn"
    );
    assert_eq!(
        lines.snapshot(),
        [format!(
            "clauth: skipping the plugin heal because this process is still running the \
             old binary ({})",
            recorded.display()
        )],
        "the detached stand-down is said once, naming the recorded path"
    );
}

/// A box that holds only another plugin's registration (no clauth row, no
/// clauth marketplace entry, no marker) reads Clean: the gate stays shut and
/// the hook spawns nothing. This pins agentgear's `installed_entry` answering
/// the asked-for plugin, never a neighbouring row.
#[cfg(unix)]
#[test]
fn self_heal_stays_clean_when_only_another_plugin_is_registered() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);

    // No install: seed only another plugin's user row and marketplace entry.
    let plugins_dir = config.join("plugins");
    std::fs::create_dir_all(&plugins_dir).expect("plugins dir");
    std::fs::write(
        plugins_dir.join("known_marketplaces.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "other": {
                "source": {"source": "directory", "path": "/nowhere/other"},
                "installLocation": "/nowhere/other",
                "lastUpdated": "2026-10-26T00:00:00.000Z"
            }
        }))
        .expect("seed json"),
    )
    .expect("seed known_marketplaces.json");
    std::fs::write(
        plugins_dir.join("installed_plugins.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "plugins": {
                "other@other": [{"scope": "user", "installPath": "/nowhere/other/plugins/cache"}]
            }
        }))
        .expect("seed json"),
    )
    .expect("seed installed_plugins.json");

    assert!(
        !super::preflight_gate(),
        "a registry holding only another plugin must read Clean"
    );
    let baseline = fake.log();
    super::self_heal().expect("hook run");
    assert_eq!(
        fake.log(),
        baseline,
        "only another plugin registered must spawn nothing from the hook"
    );
}

/// The decision's own pin: a healthy registration costs a heal caller nothing —
/// the pre-check reads Clean and the heal stays out.
#[cfg(unix)]
#[test]
fn preflight_gate_stays_shut_on_a_healthy_registration() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let _fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);
    assert!(
        !super::preflight_gate(),
        "a registration at the materialized pointer with its manifest and marker present must not heal"
    );
}

/// A binary upgrade leaves the registered plugin one version behind; only the
/// heal moves CC onto the new tree, so the pre-check must read that stale
/// version as Needed.
#[cfg(unix)]
#[test]
fn preflight_gate_heals_a_plugin_registered_at_an_older_version() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let _fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);

    let installed_path = config.join("plugins").join("installed_plugins.json");
    let mut installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&installed_path).expect("read registry"))
            .expect("registry parses");
    installed["plugins"]["clauth@clauth"][0]["version"] = serde_json::json!("0.0.1");
    std::fs::write(
        &installed_path,
        serde_json::to_vec_pretty(&installed).expect("seed json"),
    )
    .expect("seed stale registry");
    assert!(
        super::preflight_gate(),
        "a registration older than this binary's plugin must heal"
    );
}

/// ABSENT registry files are the never-installed box, not "cannot tell": a
/// config dir that has never held a plugin has no `plugins/` directory at all,
/// so the pre-check reads Clean (no entry, no marker) and spawns nothing.
#[cfg(unix)]
#[test]
fn preflight_gate_stays_shut_when_the_registry_files_are_absent() {
    use crate::testutil::{FakeClaude, HomeSandbox};

    let home = HomeSandbox::new();
    let _fake = FakeClaude::new(&home);
    assert!(
        !super::preflight_gate(),
        "no plugins dir at all is a never-installed box, not a broken one"
    );
}

// ── the detached heal ──────────────────────────────────────────────────────

/// The detached heal's "only when the pre-check says heal" half: a healthy
/// registration makes `heal_detached()` a no-op that spawns no `claude`. This is
/// the shape `clauth mcp` and the daemon hit on a working box.
#[cfg(unix)]
#[test]
fn heal_detached_skips_when_the_gate_says_healthy() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox, join_background_tasks};

    // Without this the throttle any earlier test in the process stamped refuses
    // the heal on its own, and the assertion below goes green over a gate that
    // spawns on every call. Tests share one process under `cargo test`.
    let home = HomeSandbox::new();
    super::reset_heal_throttle_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    let fake = FakeClaude::new(&home);
    crate::testutil::healthy_install(&config);
    let baseline = fake.log();

    super::heal_detached();
    join_background_tasks();
    assert_eq!(
        fake.log(),
        baseline,
        "a healthy registration must spawn no claude, got:\n{}",
        fake.log()
    );
}

/// The throttle the detached heal holds so no call site can forget it: one heal
/// may run, and a fresh attempt is refused within the 30-minute floor whether the
/// attempt succeeded or failed. The spawn decision is synchronous (the atomics
/// are set on the calling thread before any worker spawns), so pinning the
/// timestamps is deterministic. The shim log is the second half: it reds if the
/// worker stops reaching the heal at all, which a timestamp assertion alone
/// cannot see.
#[cfg(unix)]
#[test]
fn heal_detached_throttles_to_one_heal_per_window() {
    use std::sync::atomic::Ordering;

    use crate::testutil::{ConfigDirSandbox, FakeClaude, HomeSandbox, join_background_tasks};

    let home = HomeSandbox::new();
    super::reset_heal_throttle_for_test();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = ConfigDirSandbox::new(&home, &config);
    // The worker shells out for real, so the fake `claude` must be on `PATH`
    // before the spawn — without it the heal reaches the operator's own binary.
    let fake = FakeClaude::new(&home);

    // Broken: a user-scope row whose installPath is gone, so the gate says heal.
    let dir = config.join("plugins");
    std::fs::create_dir_all(&dir).expect("plugins dir");
    std::fs::write(dir.join("known_marketplaces.json"), "{}").expect("marketplaces");
    std::fs::write(
        dir.join("installed_plugins.json"),
        serde_json::to_vec(
            &serde_json::json!({
                "version": 2,
                "plugins": {"clauth@clauth": [{"scope": "user", "installPath": "/gone/runtime/plugins/cache"}]}
            }),
        )
        .expect("seed json"),
    )
    .expect("installed");

    // First attempt: the spawn path is entered, so the floor timestamp advances.
    super::heal_detached();
    assert!(
        super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed) > 0,
        "the first heal must enter the spawn path"
    );
    join_background_tasks();
    let first_start = super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed);
    let after_first = fake.log();
    assert!(
        !after_first.is_empty(),
        "the worker must actually reach the heal and spawn `claude`"
    );
    // Advance past the millisecond resolution of `now_ms` so a broken floor
    // (which would re-stamp the timestamp) can't hide behind a same-ms collision.
    std::thread::sleep(std::time::Duration::from_millis(5));

    // A fresh attempt within the window is refused before spawning: the floor
    // timestamp does not advance.
    super::heal_detached();
    assert_eq!(
        super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed),
        first_start,
        "the 30-minute floor must refuse a fresh attempt"
    );
    join_background_tasks();
    assert_eq!(
        fake.log(),
        after_first,
        "a refused attempt must spawn nothing, got:\n{}",
        fake.log()
    );

    // An in-flight heal also refuses. The floor is CLEARED first, so the flag is
    // the only guard left standing: with the floor still armed, disabling the
    // swap changes nothing and this block pins nothing.
    super::HEAL_THROTTLE
        .last_start_ms
        .store(0, Ordering::Relaxed);
    super::HEAL_THROTTLE
        .in_flight
        .store(true, Ordering::Release);
    super::heal_detached();
    assert_eq!(
        super::HEAL_THROTTLE.last_start_ms.load(Ordering::Relaxed),
        0,
        "an in-flight heal must refuse before it stamps the floor"
    );
    join_background_tasks();
    assert_eq!(
        fake.log(),
        after_first,
        "an in-flight heal must spawn nothing, got:\n{}",
        fake.log()
    );
    super::HEAL_THROTTLE
        .in_flight
        .store(false, Ordering::Release);
}

/// The reset mutates process-global statics that serialize on
/// `HOME_TEST_LOCK`, so it must refuse to run with no sandbox held.
#[cfg(all(unix, debug_assertions))]
#[test]
#[should_panic(expected = "call under a `HomeSandbox`")]
fn reset_heal_throttle_without_a_sandbox_panics() {
    super::reset_heal_throttle_for_test();
}

/// The committed root marketplace manifest must stay agentgear-conformant, or a
/// directory-source install silently drifts. The marketplace name is the install
/// key, the single entry's `source` names the embedded tree, and — agentgear
/// versioning rule 1 — a marketplace entry must NOT carry its own `version`
/// (that would mask drift in `plugins/.claude-plugin/plugin.json`, the one
/// version that must equal the crate).
#[test]
fn committed_root_marketplace_matches_agentgear_rules() {
    let marketplace: serde_json::Value =
        serde_json::from_str(include_str!("../../.claude-plugin/marketplace.json"))
            .expect("marketplace.json parses");
    assert_eq!(marketplace["name"].as_str(), Some("clauth"));
    let plugins = marketplace["plugins"]
        .as_array()
        .expect("plugins is an array");
    assert_eq!(plugins.len(), 1, "exactly one marketplace entry");
    let entry = &plugins[0];
    assert_eq!(entry["name"].as_str(), Some("clauth"));
    assert_eq!(entry["source"].as_str(), Some("./plugins"));
    assert!(
        entry.get("version").is_none(),
        "a marketplace-entry version would mask drift (agentgear versioning rule 1)"
    );

    let plugin: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/.claude-plugin/plugin.json"))
            .expect("plugin.json parses");
    assert_eq!(plugin["name"].as_str(), Some("clauth"));
    assert_eq!(plugin["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
}

// ── the installPath convergence leg ────────────────────────────────────────

/// Encode a native path spelling into the JSON string interior a registry
/// stores (a windows profiles dir carries single backslashes; serde_json
/// escapes every `\` and `"`). Independent of the product's `json_encode` so
/// a bug there cannot mask one here.
fn encode_interior(decoded: &str) -> String {
    let quoted = serde_json::to_string(decoded).expect("a &str encodes");
    quoted[1..quoted.len() - 1].to_string()
}

/// Decode a raw JSON string interior back into the value it spells.
/// Independent of the product's `json_decode`.
fn decode_interior(raw: &str) -> String {
    serde_json::from_str(&format!("\"{raw}\"")).expect("a JSON string interior")
}

/// The auto-fix's acceptance shape, pinned: plant two runtime-prefixed
/// installPaths, one with a `~/.claude/plugins/cache/` twin and one without;
/// the pass reads the first as its twin, names the second, leaves a
/// still-resolving path alone, and never reformats the file.
#[test]
fn repoint_registry_reroots_a_dead_path_and_names_a_missing_twin() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let clauth = home.home().join(".clauth");

    // The twin of the first recorded path exists; the second's does not. The
    // joins are component-wise, matching the product's per-component twin
    // build, so fixture and product spellings are byte-equal on every
    // platform — a one-string join with a mixed-separator suffix renders
    // differently on windows and breaks the byte compare.
    let twin = claude
        .join("plugins")
        .join("cache")
        .join("agenticat")
        .join("agents")
        .join("a6261ea74c14");
    std::fs::create_dir_all(&twin).expect("twin dir");

    let dead = encode_interior(&format!(
        "{}/D0/runtime-700698-0/plugins/cache/agenticat/agents/a6261ea74c14",
        clauth.join("profiles").display()
    ));
    let missing = encode_interior(&format!(
        "{}/D0/runtime-672416-5/plugins/cache/claude-plugins-official/security-guidance/2.0.8",
        clauth.join("profiles").display()
    ));
    // A live tree still resolves its recorded path: left alone today, it
    // converges the day the tree dies.
    let live = encode_interior(&format!(
        "{}/D0/runtime-9-0/plugins/cache/live/1",
        clauth.join("profiles").display()
    ));
    std::fs::create_dir_all(clauth.join("profiles/D0/runtime-9-0/plugins/cache/live/1"))
        .expect("live tree");

    let original = format!(
        "{{\n  \"version\": 2,\n  \"plugins\": {{\n    \"agents@agenticat\": [\n      {{ \"scope\": \"user\", \"installPath\": \"{dead}\" }}\n    ],\n    \"security-guidance@claude-plugins-official\": [\n      {{ \"scope\": \"user\", \"installPath\": \"{missing}\" }}\n    ],\n    \"live@live\": [\n      {{ \"scope\": \"user\", \"installPath\": \"{live}\" }}\n    ]\n  }}\n}}\n"
    );
    let registry = claude.join("plugins").join("installed_plugins.json");
    std::fs::create_dir_all(registry.parent().unwrap()).expect("plugins dir");
    std::fs::write(&registry, &original).expect("registry");

    let outcome = super::repoint_registry().expect("repoint");
    assert!(outcome.changed, "a rewrite counts as a change");
    let line = outcome.line.expect("a change reports");

    let twin_str = twin.display().to_string();
    assert!(
        line.contains("re-pointed") && line.contains(&twin_str),
        "the rewrite is named: {line}"
    );
    assert!(
        line.contains("left") && line.contains("no twin"),
        "the missing twin is named: {line}"
    );
    assert!(
        !line.contains("live/1"),
        "a still-resolving path is neither rewritten nor named: {line}"
    );
    let expected = original.replace(&dead, &encode_interior(&twin_str));
    assert_eq!(
        std::fs::read_to_string(&registry).unwrap(),
        expected,
        "only the dead path's value changes; formatting and every other byte survive"
    );
}

/// A skip-only registry still names the unconverged path: "names each one it
/// cannot" holds even when nothing was rewritten, and the naming does not
/// count as a change (no bytes moved).
#[test]
fn repoint_registry_names_a_skip_only_pass() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let clauth = home.home().join(".clauth");

    let missing = encode_interior(&format!(
        "{}/D0/runtime-672416-5/plugins/cache/claude-plugins-official/security-guidance/2.0.8",
        clauth.join("profiles").display()
    ));
    let original = format!(
        "{{\n  \"version\": 2,\n  \"plugins\": {{\n    \"security-guidance@claude-plugins-official\": [\n      {{ \"scope\": \"user\", \"installPath\": \"{missing}\" }}\n    ]\n  }}\n}}\n"
    );
    let registry = claude.join("plugins/installed_plugins.json");
    std::fs::create_dir_all(registry.parent().unwrap()).expect("plugins dir");
    std::fs::write(&registry, &original).expect("registry");
    let before = std::fs::metadata(&registry).unwrap().modified().unwrap();

    let outcome = super::repoint_registry().expect("repoint");
    assert!(
        !outcome.changed,
        "a skip moves no bytes and is not a change"
    );
    let line = outcome.line.expect("the skip names itself");
    assert!(
        line.contains("left") && line.contains("no twin"),
        "the skip-only pass names the unconverged path: {line}"
    );
    assert_eq!(
        std::fs::read_to_string(&registry).unwrap(),
        original,
        "no write"
    );
    assert_eq!(
        std::fs::metadata(&registry).unwrap().modified().unwrap(),
        before,
        "no write (mtime)"
    );
}

/// The skip line names the path a user would type, not the registry's JSON
/// spelling of it: a recorded path holding a character JSON escapes (a `"`
/// here; every `\` of a windows path alike) reads decoded in the line, the
/// same as a rewrite's two paths do.
#[test]
fn repoint_registry_names_a_skipped_path_decoded() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let profiles = home.home().join(".clauth").join("profiles");

    let dead = format!(
        "{}/D0/runtime-1-0/plugins/cache/mk/we\"ird/1",
        profiles.display()
    );
    let stored = serde_json::to_string(&dead).expect("a &str encodes");
    let original = format!(
        "{{\n  \"plugins\": {{\n    \"we@mk\": [\n      {{ \"installPath\": {stored} }}\n    ]\n  }}\n}}\n"
    );
    let registry = claude.join("plugins").join("installed_plugins.json");
    std::fs::create_dir_all(registry.parent().unwrap()).expect("plugins dir");
    std::fs::write(&registry, &original).expect("registry");

    let twin = claude
        .join("plugins")
        .join("cache")
        .join("mk")
        .join("we\"ird")
        .join("1");
    let expected = format!(
        "clauth self-heal: left {dead} (no twin at {})",
        twin.display()
    );
    let outcome = super::repoint_registry().expect("repoint");
    assert!(!outcome.changed, "a skip moves no bytes");
    assert_eq!(outcome.line.as_deref(), Some(expected.as_str()));
    assert_eq!(std::fs::read_to_string(&registry).unwrap(), original);
}

/// A control character the registry spells as a JSON escape decodes into the
/// path but prints escaped: the line reaches a terminal, and the registry is
/// another program's file.
#[test]
fn repoint_registry_escapes_a_decoded_control_character() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let profiles = home.home().join(".clauth").join("profiles");

    let dead = format!(
        "{}/D0/runtime-1-0/plugins/cache/mk/a\u{1b}[31m/1",
        profiles.display()
    );
    let stored = serde_json::to_string(&dead).expect("a &str encodes");
    let registry = claude.join("plugins").join("installed_plugins.json");
    std::fs::create_dir_all(registry.parent().unwrap()).expect("plugins dir");
    std::fs::write(
        &registry,
        format!("{{\"plugins\":{{\"a@mk\":[{{\"installPath\":{stored}}}]}}}}"),
    )
    .expect("registry");

    let twin = claude
        .join("plugins")
        .join("cache")
        .join("mk")
        .join("a\\u{1b}[31m")
        .join("1");
    let shown = dead.replace('\u{1b}', "\\u{1b}");
    let expected = format!(
        "clauth self-heal: left {shown} (no twin at {})",
        twin.display()
    );
    let line = super::repoint_registry()
        .expect("repoint")
        .line
        .expect("the skip names itself");
    assert_eq!(line, expected);
    assert!(!line.contains('\u{1b}'), "no live escape byte: {line:?}");
}

/// The separator half of the windows contract: CC records `\`-spelled paths
/// on windows, and the remap must converge them exactly like `/`-spelled
/// ones. Both spellings arrive as the raw JSON interior agentgear's scanner
/// hands over (every `\` doubled) and converge to the one canonical twin the
/// product's component-wise join builds, asserted by exact display equality on
/// every platform.
#[test]
fn registry_remap_matches_both_separator_spellings() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let clauth = home.home().join(".clauth");
    let profiles = clauth.join("profiles");
    let prefix_fwd = format!("{}/", profiles.display());
    let prefix_back = format!("{}\\", profiles.display());

    let canonical = claude
        .join("plugins")
        .join("cache")
        .join("mkt")
        .join("plugin")
        .join("1");
    std::fs::create_dir_all(&canonical).expect("twin dir");

    let back_path = encode_interior(&format!(
        "{}\\D0\\runtime-9-0\\plugins\\cache\\mkt\\plugin\\1",
        profiles.display()
    ));
    match super::registry_remap(&back_path, &prefix_fwd, &prefix_back, &claude) {
        agentgear::Remap::Rewrite(to) => assert_eq!(
            decode_interior(&to),
            canonical.display().to_string(),
            "the backslash-spelled path rewrites to the canonical twin"
        ),
        agentgear::Remap::Keep => {
            panic!("a dead backslash-spelled path with a twin must rewrite, got Keep")
        }
        agentgear::Remap::Skip(reason) => {
            panic!("a dead backslash-spelled path with a twin must rewrite, got Skip({reason})")
        }
    }

    let fwd_path = encode_interior(&format!(
        "{}/D0/runtime-9-0/plugins/cache/mkt/plugin/1",
        profiles.display()
    ));
    match super::registry_remap(&fwd_path, &prefix_fwd, &prefix_back, &claude) {
        agentgear::Remap::Rewrite(to) => assert_eq!(
            decode_interior(&to),
            canonical.display().to_string(),
            "the forward-spelled path rewrites to the canonical twin"
        ),
        agentgear::Remap::Keep => {
            panic!("a dead forward-spelled path with a twin must rewrite, got Keep")
        }
        agentgear::Remap::Skip(reason) => {
            panic!("a dead forward-spelled path with a twin must rewrite, got Skip({reason})")
        }
    }

    let outside = "/home/u/.claude/plugins/cache/mkt/plugin/1";
    assert!(
        matches!(
            super::registry_remap(outside, &prefix_fwd, &prefix_back, &claude),
            agentgear::Remap::Keep
        ),
        "a path outside the profiles dir is not targeted"
    );
}

/// The windows half of the remap contract: agentgear's scanner hands the remap
/// the registry string's raw JSON interior, so on windows every `\` arrives
/// doubled, and the rewrite must go back JSON-escaped — agentgear writes it
/// verbatim between the quotes, and a raw `\` spelling would corrupt the JSON.
/// The remap therefore decodes, matches and builds the twin on the decoded
/// value, then encodes the rewrite. The windows spellings are stand-ins rooted
/// under the sandbox home: on linux `\` is an ordinary filename byte, so the
/// twin the product joins really exists and the assertions are exact (pure
/// string/JSON, no cfg(windows)).
#[cfg(unix)]
#[test]
fn registry_remap_decodes_the_raw_text_and_json_encodes_the_rewrite() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let profiles_back = format!(r"{}/C:\Users\u\.clauth\profiles", home.home().display());
    let claude_back =
        std::path::PathBuf::from(format!(r"{}/C:\Users\u\.claude", home.home().display()));
    let prefix_fwd = format!("{profiles_back}/");
    let prefix_back = format!("{profiles_back}\\");

    let canonical = claude_back
        .join("plugins")
        .join("cache")
        .join("mkt")
        .join("plugin")
        .join("1");
    std::fs::create_dir_all(&canonical).expect("twin dir");

    // The raw interior a windows registry stores: every `\` doubled.
    let raw = format!(
        r"{}/C:\\Users\\u\\.clauth\\profiles\\D0\\runtime-9-0\\plugins\\cache\\mkt\\plugin\\1",
        home.home().display()
    );
    match super::registry_remap(&raw, &prefix_fwd, &prefix_back, &claude_back) {
        agentgear::Remap::Rewrite(to) => {
            let decoded = serde_json::from_str::<String>(&format!("\"{to}\""))
                .expect("the rewrite is valid JSON string content");
            assert_eq!(
                decoded,
                canonical.display().to_string(),
                "the rewrite decodes to the canonical twin"
            );
            assert!(
                to.contains(r"\\Users"),
                "the rewrite is JSON-escaped, not a raw backslash spelling: {to}"
            );
        }
        agentgear::Remap::Keep => {
            panic!("the raw-escaped windows path must decode into a rewrite, got Keep")
        }
        agentgear::Remap::Skip(reason) => {
            panic!("the raw-escaped windows path must decode into a rewrite, got Skip({reason})")
        }
    }
}

/// A registry whose `installPath` holds the raw
/// JSON-escaped windows spelling (`\\` per `\`) converges to the new prefix
/// through the exact call [`super::repoint_registry`] makes, and the rewritten
/// bytes still parse as JSON with the decoded `installPath` at the twin.
// The windows stand-in paths (`\` as a filename byte, an embedded `C:`) are
// invalid paths on windows itself, where the shape runs for real.
#[cfg(unix)]
#[test]
fn repoint_converges_a_json_escaped_windows_registry_and_the_bytes_still_parse() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let profiles_back = format!(r"{}/C:\Users\u\.clauth\profiles", home.home().display());
    let claude_back =
        std::path::PathBuf::from(format!(r"{}/C:\Users\u\.claude", home.home().display()));
    let prefix_fwd = format!("{profiles_back}/");
    let prefix_back = format!("{profiles_back}\\");

    let twin = claude_back
        .join("plugins")
        .join("cache")
        .join("mkt")
        .join("plugin")
        .join("1");
    std::fs::create_dir_all(&twin).expect("twin dir");

    let dead = format!(
        r"{}/C:\\Users\\u\\.clauth\\profiles\\D0\\runtime-9-0\\plugins\\cache\\mkt\\plugin\\1",
        home.home().display()
    );
    let original =
        format!(r#"{{"plugins":{{"agents@mkt":[{{"scope":"user","installPath":"{dead}"}}]}}}}"#);
    let registry = claude.join("plugins").join("installed_plugins.json");
    std::fs::create_dir_all(registry.parent().expect("plugins dir")).expect("plugins dir");
    std::fs::write(&registry, &original).expect("registry");

    let report = agentgear::repoint_install_paths(&registry, |path| {
        super::registry_remap(path, &prefix_fwd, &prefix_back, &claude_back)
    })
    .expect("repoint");
    assert_eq!(
        report.rewritten.len(),
        1,
        "the JSON-escaped windows path converges: {report:?}"
    );
    let written = std::fs::read_to_string(&registry).expect("registry");
    let parsed: serde_json::Value =
        serde_json::from_str(&written).expect("the rewritten bytes parse as JSON");
    assert_eq!(
        parsed["plugins"]["agents@mkt"][0]["installPath"]
            .as_str()
            .expect("installPath is a string"),
        twin.display().to_string(),
        "the decoded installPath is the canonical twin"
    );
}

/// The re-point line names the paths a human recognizes: agentgear reports the
/// rewrite with the raw interior as `from` and the JSON-escaped spelling as
/// `to` (on windows both carry doubled backslashes), so the line decodes both
/// and falls back to the raw text when a decode fails. Pinned by equality on
/// the whole line — any raw/decoded mix breaks it.
#[cfg(unix)]
#[test]
fn repoint_line_names_decoded_spellings() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");

    let twin = claude
        .join("plugins")
        .join("cache")
        .join("mkt")
        .join("plugin")
        .join("1");
    std::fs::create_dir_all(&twin).expect("twin dir");

    // The escaped interior agentgear hands back as `Repointed.from`: every `\`
    // doubled. The sandbox's profile prefix is forward-spelled, so the doubled
    // run starts after it — the same "escaped interior" shape the remap sees
    // on windows, and the backslash tail keeps the decoded path dangling.
    let raw_from = format!(
        r"{}/.clauth/profiles\\D0\\runtime-9-0\\plugins\\cache\\mkt\\plugin\\1",
        home.home().display()
    );
    let decoded_from = format!(
        r"{}/.clauth/profiles\D0\runtime-9-0\plugins\cache\mkt\plugin\1",
        home.home().display()
    );
    let original = format!(
        r#"{{"plugins":{{"agents@mkt":[{{"scope":"user","installPath":"{raw_from}"}}]}}}}"#
    );
    let registry = claude.join("plugins").join("installed_plugins.json");
    std::fs::create_dir_all(registry.parent().expect("plugins dir")).expect("plugins dir");
    std::fs::write(&registry, &original).expect("registry");

    let outcome = super::repoint_registry().expect("repoint");
    assert!(outcome.changed, "the rewrite counts as a change");
    let expected = format!(
        "clauth self-heal: re-pointed {decoded_from} -> {}",
        twin.display()
    );
    assert_eq!(
        outcome.line.expect("a rewrite reports"),
        expected,
        "the line names the decoded spellings, not the JSON-escaped raw interior"
    );
}

/// The decode-failure guard: a raw interior the JSON string grammar cannot
/// read (an invalid escape like `\D`, or a raw control byte) is text the remap
/// must not act on — it Keeps, and the pass moves no bytes, even though the
/// raw text itself matches the backslash prefix a corrupt registry can carry.
#[test]
fn registry_remap_keeps_an_unreadable_interior() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let profiles = home.home().join(".clauth").join("profiles");
    let prefix_fwd = format!("{}/", profiles.display());
    let prefix_back = format!("{}\\", profiles.display());

    // A raw single-`\` interior: invalid JSON string content (`\D`), yet the
    // raw text matches the backslash prefix on every platform, so only the
    // decode guard stands between the matcher and the unreadable value.
    let bad_escape = format!(
        r"{}\D0\runtime-9-0\plugins\cache\mkt\plugin\1",
        profiles.display()
    );
    assert!(
        bad_escape.starts_with(&prefix_back),
        "the fixture reaches the matcher"
    );
    assert!(
        matches!(
            super::registry_remap(&bad_escape, &prefix_fwd, &prefix_back, &claude),
            agentgear::Remap::Keep
        ),
        "an interior with an invalid JSON escape is unreadable: Keep"
    );

    // A raw control byte inside the interior is equally unreadable.
    let control_byte = format!(
        "{}/\u{1}D0/runtime-9-0/plugins/cache/mkt/plugin/1",
        profiles.display()
    );
    assert!(
        control_byte.starts_with(&prefix_fwd),
        "the fixture reaches the matcher"
    );
    assert!(
        matches!(
            super::registry_remap(&control_byte, &prefix_fwd, &prefix_back, &claude),
            agentgear::Remap::Keep
        ),
        "an interior with a raw control byte is unreadable: Keep"
    );

    // And the pass moves no bytes on the unreadable interior.
    let original = format!(
        r#"{{"plugins":{{"agents@mkt":[{{"scope":"user","installPath":"{bad_escape}"}}]}}}}"#
    );
    let registry = claude.join("plugins").join("installed_plugins.json");
    std::fs::create_dir_all(registry.parent().expect("plugins dir")).expect("plugins dir");
    std::fs::write(&registry, &original).expect("registry");
    let report = agentgear::repoint_install_paths(&registry, |path| {
        super::registry_remap(path, &prefix_fwd, &prefix_back, &claude)
    })
    .expect("repoint");
    assert!(
        report.rewritten.is_empty() && report.skipped.is_empty(),
        "the unreadable interior is neither rewritten nor named: {report:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&registry).expect("registry"),
        original,
        "the pass moves no bytes on an unreadable interior"
    );
}

/// The detached leg reports a skip-only line once per process: the daemon
/// tick would otherwise repeat the same line every tick until a re-login
/// lands the twin.
#[test]
fn detached_repoint_reports_skips_once_per_process() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    let clauth = home.home().join(".clauth");

    let missing = encode_interior(&format!(
        "{}/D0/runtime-672416-5/plugins/cache/claude-plugins-official/security-guidance/2.0.8",
        clauth.join("profiles").display()
    ));
    let registry = claude.join("plugins/installed_plugins.json");
    std::fs::create_dir_all(registry.parent().unwrap()).expect("plugins dir");
    std::fs::write(
        &registry,
        format!(r#"{{"version":2,"plugins":{{"x@x":[{{"installPath":"{missing}"}}]}}}}"#),
    )
    .expect("registry");

    let first = super::detached_repoint_line().expect("first pass");
    assert!(first.is_some(), "the first pass names the skip: {first:?}");
    let second = super::detached_repoint_line().expect("second pass");
    assert_eq!(second, None, "the second pass stays silent until reset");
    super::reset_skip_report_for_test();
    let third = super::detached_repoint_line().expect("third pass");
    assert!(third.is_some(), "the reset re-arms the report");
}

/// The committed install script runs the heal after both install legs, so a
/// binary install converges dangling plugin paths without waiting for the
/// next session start. A drift here silently drops the fix.
#[test]
fn install_sh_runs_self_heal_after_both_install_legs() {
    let script = include_str!("../../install.sh");
    assert_eq!(
        script.matches("self-heal").count(),
        2,
        "install.sh must run `clauth self-heal` after the cargo leg and after the download leg: {script}"
    );
    assert!(
        script.contains("cargo install clauth")
            && script.contains("\"${INSTALL_DIR}/${BINARY}\" self-heal"),
        "both legs carry the heal call"
    );
}

/// The silent contract: a clean or absent registry prints nothing and moves
/// no bytes, so a session start stays quiet.
#[test]
fn repoint_registry_says_nothing_when_clean() {
    use crate::testutil::HomeSandbox;

    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");

    // Absent registry: nothing to converge, nothing created.
    let registry = claude.join("plugins/installed_plugins.json");
    let outcome = super::repoint_registry().expect("repoint");
    assert!(
        outcome.line.is_none(),
        "no registry, nothing to say: {outcome:?}"
    );
    assert!(!registry.exists(), "a missing registry is not created");

    // Clean registry: no targets, no write.
    let bytes = r#"{
  "version": 2,
  "plugins": {
    "widget@widget": [
      { "scope": "user", "installPath": "CLAUDE_TWIN_PLACEHOLDER" }
    ]
  }
}
"#
    .replace(
        "CLAUDE_TWIN_PLACEHOLDER",
        &encode_interior(
            &claude
                .join("plugins/cache/widget/widget/0.5.1")
                .display()
                .to_string(),
        ),
    );
    std::fs::create_dir_all(registry.parent().unwrap()).expect("plugins dir");
    std::fs::write(&registry, &bytes).expect("registry");
    let before = std::fs::metadata(&registry).unwrap().modified().unwrap();

    let outcome = super::repoint_registry().expect("repoint");
    assert!(
        outcome.line.is_none(),
        "nothing targeted, nothing to say: {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&registry).unwrap(),
        bytes,
        "no write"
    );
    assert_eq!(
        std::fs::metadata(&registry).unwrap().modified().unwrap(),
        before,
        "no write (mtime)"
    );
}

/// The query the feed build and the TUI ask answers the heal's verdict but
/// says nothing: neither the stand-down line nor the fail-open note, which
/// both speak for a heal that is not running.
#[cfg(unix)]
#[test]
fn the_replaced_query_reports_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    super::reset_stand_down_report_for_test();
    super::reset_record_failure_report_for_test();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let recorded = crate::testutil::write_shim(home.home(), "self-exe-recorded", "exit 0");
    {
        let _self = super::SelfExeOverride::pin(Some(recorded.as_path()));
        assert_eq!(
            super::self_exe_change(),
            None,
            "an unchanged binary reads unchanged"
        );
        let replacement =
            crate::testutil::write_shim(home.home(), "self-exe-replacement", "exit 0 # replaced");
        std::fs::rename(&replacement, &recorded).expect("rename-replace the recorded file");
        assert_eq!(
            super::self_exe_change(),
            Some(super::ExeChange::Replaced),
            "a replaced binary reads replaced"
        );
        assert_eq!(
            super::self_exe_change(),
            Some(super::ExeChange::Replaced),
            "and reads replaced again"
        );
        std::fs::remove_file(&recorded).expect("remove the recorded file");
        assert_eq!(
            super::self_exe_change(),
            Some(super::ExeChange::Removed),
            "a path left empty reads removed"
        );
        std::fs::create_dir(&recorded).expect("a directory at the recorded path");
        assert_eq!(
            super::self_exe_change(),
            Some(super::ExeChange::Replaced),
            "something other than a file at the path reads replaced, never removed"
        );
    }
    {
        let _self = super::SelfExeOverride::pin(None);
        assert_eq!(
            super::self_exe_change(),
            None,
            "a failed recording reads as unchanged"
        );
    }
    assert_eq!(
        lines.snapshot(),
        Vec::<String>::new(),
        "the query logs nothing"
    );
}
