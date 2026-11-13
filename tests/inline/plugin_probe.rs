//! Inline tests for `plugin_probe`. Home-sandboxed (each grabs `HOME_TEST_LOCK`
//! via `HomeSandbox`) so registry / `~/.claude.json` reads and writes never touch
//! the real `~`.

use super::*;
use std::fs;

use serde_json::{Value, json};

use crate::profile::{claude_dir, home_dir};
use crate::testutil::HomeSandbox;

fn write_json(path: &std::path::Path, value: &Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
}

fn plugins_path(file: &str) -> std::path::PathBuf {
    claude_dir().expect("claude dir").join("plugins").join(file)
}

#[test]
fn installed_records_parses_entry() {
    let _home = HomeSandbox::new();
    write_json(
        &plugins_path("installed_plugins.json"),
        &json!({
            "plugins": {
                "clauth@clauth": [{
                    "scope": "user",
                    "version": "0.1.0",
                    "gitCommitSha": "aab3e45deadbeef",
                    "installedAt": "2026-06-19T00:00:00Z",
                    "installPath": "/home/u/.claude/plugins/clauth",
                }]
            }
        }),
    );

    let records = installed_records();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert_eq!(rec.scope.as_deref(), Some("user"));
    assert_eq!(rec.version.as_deref(), Some("0.1.0"));
    assert_eq!(rec.git_commit_sha.as_deref(), Some("aab3e45deadbeef"));
    assert!(rec.project_path.is_none());
}

#[test]
fn installed_records_empty_when_absent_or_other_plugin() {
    let _home = HomeSandbox::new();
    // No file at all.
    assert!(installed_records().is_empty());
    // File present but only carries an unrelated plugin.
    write_json(
        &plugins_path("installed_plugins.json"),
        &json!({ "plugins": { "other@market": [{ "scope": "user" }] } }),
    );
    assert!(installed_records().is_empty());
}

#[test]
fn marketplace_known_parses_source_repo() {
    let _home = HomeSandbox::new();
    write_json(
        &plugins_path("known_marketplaces.json"),
        &json!({
            "clauth": {
                "source": { "source": "github", "repo": "uwuclxdy/clauth" },
                "installLocation": "/home/u/.claude/plugins/marketplaces/clauth"
            }
        }),
    );

    let info = marketplace_known().expect("marketplace present");
    assert_eq!(info.repo.as_deref(), Some("uwuclxdy/clauth"));
    assert!(info.install_location.is_some());
    // Missing file → None.
    fs::remove_file(plugins_path("known_marketplaces.json")).expect("rm");
    assert!(marketplace_known().is_none());
}

#[test]
fn manual_mcp_wiring_detects_global_config() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    // No file → not wired.
    assert_eq!(manual_mcp_wiring(), McpWiring::None);

    write_json(
        &path,
        &json!({ "mcpServers": { "clauth": { "command": "clauth", "args": ["mcp"] } } }),
    );
    assert_eq!(manual_mcp_wiring(), McpWiring::GlobalConfig);
}

#[test]
fn wire_mcp_server_writes_entry_and_preserves_other_fields() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    write_json(&path, &json!({ "userID": "abc123", "tips": { "x": 1 } }));

    wire_mcp_server().expect("wire");

    let root: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
    // Other fields untouched.
    assert_eq!(root.get("userID").and_then(Value::as_str), Some("abc123"));
    assert!(root.get("tips").is_some());
    // The clauth entry matches the plugin manifest's stdio shape.
    let entry = &root["mcpServers"]["clauth"];
    assert_eq!(entry["command"].as_str(), Some("clauth"));
    assert_eq!(entry["args"][0].as_str(), Some("mcp"));
    assert_eq!(entry["type"].as_str(), Some("stdio"));
    // And the file now reads as wired.
    assert_eq!(manual_mcp_wiring(), McpWiring::GlobalConfig);
}

#[test]
fn wire_mcp_server_preserves_other_servers() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    write_json(
        &path,
        &json!({ "mcpServers": { "other": { "command": "x", "args": [] } } }),
    );

    wire_mcp_server().expect("wire");

    let root: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
    // The pre-existing server must survive alongside the new clauth entry.
    assert_eq!(root["mcpServers"]["other"]["command"].as_str(), Some("x"));
    assert_eq!(
        root["mcpServers"]["clauth"]["command"].as_str(),
        Some("clauth")
    );
}

#[test]
fn wire_mcp_server_replaces_non_object_mcpservers() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    // A malformed mcpServers (not an object) must be replaced, not error out.
    write_json(&path, &json!({ "mcpServers": "garbage", "userID": "keep" }));

    wire_mcp_server().expect("wire");

    let root: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
    assert_eq!(
        root["mcpServers"]["clauth"]["command"].as_str(),
        Some("clauth")
    );
    assert_eq!(root.get("userID").and_then(Value::as_str), Some("keep"));
}

#[test]
fn wire_mcp_server_creates_file_when_absent() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    assert!(!path.exists());

    wire_mcp_server().expect("wire");

    assert!(path.exists());
    assert_eq!(manual_mcp_wiring(), McpWiring::GlobalConfig);
}

#[test]
fn global_entry_drifted_flags_stale_command_and_args() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");

    // No entry → nothing to validate.
    assert_eq!(global_entry_drifted(), None);

    // Canonical entry (what the wire fix writes) → no drift.
    wire_mcp_server().expect("wire");
    assert_eq!(global_entry_drifted(), Some(false));

    // A stale absolute command no longer matches the launch line → drift.
    write_json(
        &path,
        &json!({ "mcpServers": { "clauth": { "command": "/old/bin/clauth", "args": ["mcp"] } } }),
    );
    assert_eq!(global_entry_drifted(), Some(true));

    // Args missing the `mcp` subcommand → drift.
    write_json(
        &path,
        &json!({ "mcpServers": { "clauth": { "command": "clauth", "args": [] } } }),
    );
    assert_eq!(global_entry_drifted(), Some(true));
}

/// Both `_meta` keys are required on every request now that no handshake
/// carries them. Drop one and a conforming server answers `-32602`, which the
/// probe would report as a broken clauth.
#[test]
fn the_probe_frame_is_a_well_formed_stateless_opener() {
    let frame = discover_frame();

    assert_eq!(frame["method"].as_str(), Some("server/discover"));
    let meta = &frame["params"]["_meta"];
    assert_eq!(
        meta["io.modelcontextprotocol/protocolVersion"].as_str(),
        Some("2026-07-28")
    );
    assert!(
        meta["io.modelcontextprotocol/clientCapabilities"].is_object(),
        "clientCapabilities is required, got: {meta}"
    );
}

/// A discover result whose capabilities carry no `tools` is the failure a
/// forced `tools/list` cannot see: the server answers, and the client still
/// exposes nothing.
#[test]
fn the_discover_probe_needs_an_advertised_tools_capability() {
    let with_tools = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "resultType": "complete", "capabilities": { "tools": {} } }
    });
    assert_eq!(parse_discover_reply(&with_tools.to_string()), McpProbe::Ok);

    let without = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "resultType": "complete", "capabilities": {} }
    });
    assert_eq!(
        parse_discover_reply(&without.to_string()),
        McpProbe::Failed("server advertises no tools capability".to_string())
    );
}

/// A `clauth` too old to know `server/discover` answers method-not-found, and
/// the tab's one line of reason is the only place that can say so.
#[test]
fn the_discover_probe_names_a_pre_stateless_binary() {
    let old = json!({
        "jsonrpc": "2.0", "id": 1,
        "error": { "code": -32601, "message": "Method not found" }
    });
    let McpProbe::Failed(reason) = parse_discover_reply(&old.to_string()) else {
        panic!("a method-not-found reply must fail the probe");
    };
    assert!(
        reason.contains("predates the stateless protocol"),
        "reason must name the cause, got: {reason}"
    );

    // Any other error keeps the server's own message.
    let other = json!({
        "jsonrpc": "2.0", "id": 1,
        "error": { "code": -32022, "message": "Unsupported protocol version" }
    });
    let McpProbe::Failed(reason) = parse_discover_reply(&other.to_string()) else {
        panic!("an error reply must fail the probe");
    };
    assert!(
        reason.contains("Unsupported protocol version"),
        "reason must carry the server's message, got: {reason}"
    );
}

/// The probe's child is a full `clauth mcp` server, so it must say who spawned
/// it: without this the handshake registers a bare session for its own lifetime
/// and the live tally counts a session nobody is running.
#[test]
fn the_mcp_probe_marks_its_child_as_a_probe() {
    let env = crate::testutil::env_overrides(&probe_command());

    assert_eq!(
        env.get(crate::mcp::MCP_PROBE_ENV),
        Some(&Some("1".to_string())),
        "the probe's child must be marked, got: {env:?}"
    );
}

/// A test build resolves `on_path` only inside the active `HomeSandbox`: a
/// binary installed on the box (a `clauth-*-proxy` in `~/.local/bin`) never
/// answers a test's lookup, while a stub the test put under its sandbox does.
#[test]
fn a_test_build_never_resolves_a_binary_outside_the_sandbox() {
    let home = HomeSandbox::new();
    let outside = tempfile::tempdir().expect("outside dir");
    let inside = home.home().join("bin");
    fs::create_dir_all(&inside).expect("mkdir inside");
    let name = "clauth-pathpin-proxy";
    for dir in [outside.path(), inside.as_path()] {
        let bin = dir.join(name);
        fs::write(&bin, "#!/bin/sh\n").expect("write binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    let only_outside = std::env::join_paths([outside.path()]).expect("join");
    {
        let _path =
            crate::testutil::EnvPin::new(&home, &[("PATH", Some(only_outside.as_os_str()))]);
        assert_eq!(
            on_path(name),
            None,
            "a binary outside the sandbox never resolves in a test build"
        );
    }

    let both = std::env::join_paths([outside.path(), inside.as_path()]).expect("join");
    let _path = crate::testutil::EnvPin::new(&home, &[("PATH", Some(both.as_os_str()))]);
    assert_eq!(
        on_path(name),
        Some(inside.join(name)),
        "a stub under the sandbox still resolves"
    );
}

/// A home `.claude.json` that does not parse (Claude Code mid-write, or a torn
/// file) is never replaced: wiring refuses and leaves the bytes as they were,
/// so no pass propagates a home that lost every other field.
#[test]
fn wiring_never_replaces_an_unparseable_home() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    let torn = b"{\"numStartups\": 3, \"projects\": {\"/a\": {\"x\"".to_vec();
    fs::write(&path, &torn).expect("write torn home");

    assert!(wire_mcp_server().is_err(), "wiring a torn home must refuse");
    assert_eq!(
        fs::read(&path).expect("read"),
        torn,
        "the torn home is left exactly as it was"
    );
}

/// Wiring adds `mcpServers.clauth` to home while a runtime copy holds a newer
/// Claude Code write no pass has propagated yet: after the next pass every copy
/// carries both, the wire's entry and the runtime write.
#[test]
fn wiring_keeps_an_unpropagated_runtime_write_and_its_own_entry() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    crate::testutil::set_mtime(&global, t(10));
    crate::testutil::set_mtime(&r1, t(20));
    let _live_r1 = arm_live_copy(&r1);

    wire_mcp_server().expect("wire");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    let g = serde_json::from_slice::<Value>(&fs::read(&global).expect("read")).expect("parse");
    let r = serde_json::from_slice::<Value>(&fs::read(&r1).expect("read")).expect("parse");
    for (name, doc) in [("home", &g), ("the runtime copy", &r)] {
        assert_eq!(
            doc["numStartups"],
            json!(3),
            "{name} keeps the runtime write"
        );
        assert_eq!(doc["ccR1"], json!(true), "{name} keeps the runtime write");
        assert_eq!(
            doc["mcpServers"]["clauth"]["command"],
            json!("clauth"),
            "{name} keeps the wire's entry"
        );
    }
    assert_eq!(
        r["oauthAccount"],
        json!({"emailAddress": "r1@x"}),
        "the runtime copy keeps its own account"
    );
}

fn runtime_copy(home: &std::path::Path, profile: &str, runtime: &str) -> std::path::PathBuf {
    home.join(".clauth")
        .join("profiles")
        .join(profile)
        .join(runtime)
        .join(".claude.json")
}

/// A live `clauth start` session's flock-held marker on the sessions dir
/// `runtime::paired_sessions_name` pairs with `copy`'s runtime dir. Keep the
/// returned handle alive for as long as the copy should read as live.
fn arm_live_copy(copy: &std::path::Path) -> std::fs::File {
    let runtime_dir = copy.parent().expect("runtime dir");
    let name = runtime_dir
        .file_name()
        .expect("runtime dir name")
        .to_string_lossy()
        .replacen("runtime", "sessions", 1);
    let sessions = runtime_dir.with_file_name(name);
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("test-pid")).expect("open pid");
    pid.lock().expect("lock pid");
    pid
}

/// Write `path` as a copy clauth wrote: the production write-and-record path,
/// stamped at `when`, so the merge reads it as a recorded copy carrying that
/// write.
fn write_recorded_json(path: &std::path::Path, value: &Value, when: std::time::SystemTime) {
    let record = crate::claude_json::written_record_path().expect("record path");
    let bytes = serde_json::to_vec_pretty(value).expect("serialize");
    let since_epoch = when
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post-epoch");
    crate::jsonsync::write_copy_and_record(
        Some(&record),
        path,
        &bytes,
        (since_epoch.as_secs() as i64, since_epoch.subsec_nanos()),
        true,
        Some(when),
    )
    .expect("write recorded copy");
}

fn read_doc(path: &std::path::Path) -> Value {
    serde_json::from_slice::<Value>(&fs::read(path).expect("read")).expect("parse")
}

struct PostWriteHookGuard;

impl Drop for PostWriteHookGuard {
    fn drop(&mut self) {
        crate::jsonsync::set_post_write_hook(None);
    }
}

/// Arms the post-write hook to land `doc` as a Claude Code write on `target`
/// right after the `nth` copy write the wire makes, and counts the writes.
fn land_after_nth_write(
    target: &std::path::Path,
    doc: Value,
    nth: usize,
) -> (
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    PostWriteHookGuard,
) {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let target = target.to_path_buf();
    let counted = calls.clone();
    crate::jsonsync::set_post_write_hook(Some(Box::new(move || {
        let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if n == nth {
            write_json(&target, &doc);
            crate::testutil::set_mtime(&target, std::time::SystemTime::now());
        }
    })));
    (calls, PostWriteHookGuard)
}

/// A Claude Code write to a runtime copy landing while the wire is writing,
/// after any one of its copy writes, is never reverted: after the next pass
/// every copy carries that write.
#[test]
fn a_claude_code_write_landing_mid_wire_survives_every_pass() {
    for nth in 1..=3 {
        let home = HomeSandbox::new();
        let global = home_dir().expect("home").join(".claude.json");
        let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
        let r2 = runtime_copy(home.home(), "p2", "runtime-4242-1");
        fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
        fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
        write_json(&global, &json!({"numStartups": 2}));
        write_json(&r1, &json!({"numStartups": 3, "ccR1": true}));
        write_json(&r2, &json!({"numStartups": 1}));
        let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
        crate::testutil::set_mtime(&global, t(10));
        crate::testutil::set_mtime(&r1, t(20));
        crate::testutil::set_mtime(&r2, t(5));
        let _live_r1 = arm_live_copy(&r1);
        let _live_r2 = arm_live_copy(&r2);

        let (calls, guard) = land_after_nth_write(
            &r2,
            json!({"numStartups": 4, "ccR1": true, "ccR2": true}),
            nth,
        );
        let wired = wire_mcp_server();
        drop(guard);
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= nth,
            "write {nth}: the Claude Code write landed mid-wire ({wired:?})"
        );
        crate::claude_json::reset_last_synced();
        crate::claude_json::sync_once().expect("next pass");

        for (name, path) in [("home", &global), ("r1", &r1), ("r2", &r2)] {
            let doc = read_doc(path);
            assert_eq!(
                doc["ccR2"],
                json!(true),
                "write {nth}: {name} keeps the Claude Code write that landed mid-wire"
            );
            assert_eq!(
                doc["numStartups"],
                json!(4),
                "write {nth}: {name} keeps the Claude Code write that landed mid-wire"
            );
        }
    }
}

/// A runtime copy that is mid-write (torn) when the wire reads it makes the
/// click answer the parse line with nothing changed; once Claude Code's write
/// completes, the next pass keeps it everywhere, and a second click wires
/// every copy.
#[test]
fn a_torn_copy_refuses_the_wire_and_keeps_its_write() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    let r2 = runtime_copy(home.home(), "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
    let _live_r1 = arm_live_copy(&r1);
    let _live_r2 = arm_live_copy(&r2);
    write_json(&global, &json!({"numStartups": 2}));
    write_json(&r1, &json!({"numStartups": 3, "ccR1": true}));
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    // The copy the completed write must win with is clauth's own: the wire
    // refuses the torn member, so no other path records it.
    write_recorded_json(&r2, &json!({"numStartups": 4, "ccR1": true}), t(20));
    fs::write(&r2, b"{\"numStartups\": 4, \"ccR1\": tr").expect("torn");
    crate::testutil::set_mtime(&global, t(10));
    crate::testutil::set_mtime(&r1, t(20));
    crate::testutil::set_mtime(&r2, t(30));
    let before: Vec<Vec<u8>> = [&global, &r1, &r2]
        .iter()
        .map(|p| fs::read(p).expect("read"))
        .collect();

    let err = wire_mcp_server().expect_err("a torn copy refuses the wire");
    assert_eq!(
        err.to_string(),
        format!(
            "{} does not parse as a JSON object; nothing was changed",
            r2.display()
        ),
        "the refusal names the torn copy"
    );
    let after: Vec<Vec<u8>> = [&global, &r1, &r2]
        .iter()
        .map(|p| fs::read(p).expect("read"))
        .collect();
    assert_eq!(after, before, "a refused wire changes no copy");

    write_json(&r2, &json!({"numStartups": 4, "ccR1": true, "ccR2": true}));
    crate::testutil::set_mtime(&r2, t(40));
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");
    wire_mcp_server().expect("a second click wires every copy");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("pass after the wire");

    for (name, path) in [("home", &global), ("r1", &r1), ("r2", &r2)] {
        let doc = read_doc(path);
        assert_eq!(
            doc["ccR2"],
            json!(true),
            "{name} keeps the torn copy's completed write"
        );
        assert_eq!(
            doc["mcpServers"]["clauth"]["command"],
            json!("clauth"),
            "{name} carries the wire's entry"
        );
    }
}

struct WireAfterReadHookGuard;

impl Drop for WireAfterReadHookGuard {
    fn drop(&mut self) {
        crate::plugin_probe::set_wire_after_read_hook(None);
    }
}

/// Arms the wire's test seam that runs once after it has read every member and
/// before its first write.
fn after_wire_read(f: impl Fn() + Send + Sync + 'static) -> WireAfterReadHookGuard {
    crate::plugin_probe::set_wire_after_read_hook(Some(Box::new(f)));
    WireAfterReadHookGuard
}

/// A home Claude Code creates after the wire found none is never overwritten:
/// the wire re-reads it like a moved copy, and the next pass keeps Claude
/// Code's write everywhere.
#[test]
fn a_home_created_mid_wire_keeps_its_write() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(&r1, &json!({"numStartups": 3, "ccR1": true}));
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    crate::testutil::set_mtime(&r1, t(20));
    let _live_r1 = arm_live_copy(&r1);

    let guard = after_wire_read({
        let global = global.clone();
        move || {
            write_json(
                &global,
                &json!({"numStartups": 9, "ccR1": true, "ccHome": true}),
            );
            crate::testutil::set_mtime(&global, std::time::SystemTime::now());
        }
    });
    let wired = wire_mcp_server();
    drop(guard);
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    for (name, path) in [("home", &global), ("r1", &r1)] {
        let doc = read_doc(path);
        assert_eq!(
            doc["ccHome"],
            json!(true),
            "{name} keeps the home Claude Code created mid-wire ({wired:?})"
        );
        assert_eq!(
            doc["numStartups"],
            json!(9),
            "{name} keeps the home Claude Code created mid-wire ({wired:?})"
        );
    }
}

/// A runtime copy that disappears mid-wire (its session ended) is just
/// absent: the wire still wires every other copy and answers success.
#[test]
fn a_copy_vanishing_mid_wire_is_skipped() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    let r2 = runtime_copy(home.home(), "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
    write_json(&global, &json!({"numStartups": 2}));
    write_json(&r1, &json!({"numStartups": 2}));
    write_json(&r2, &json!({"numStartups": 2}));
    let _live_r1 = arm_live_copy(&r1);
    let _live_r2 = arm_live_copy(&r2);

    let guard = after_wire_read({
        let r1 = r1.clone();
        move || fs::remove_file(&r1).expect("the session ends")
    });
    let wired = wire_mcp_server();
    drop(guard);

    wired.expect("a vanished copy is skipped, not a refusal");
    assert!(!r1.exists(), "the vanished copy stays gone");
    for (name, path) in [("home", &global), ("r2", &r2)] {
        assert_eq!(
            read_doc(path)["mcpServers"]["clauth"]["command"],
            json!("clauth"),
            "{name} is still wired"
        );
    }
}

/// The home the wire creates ranks lowest even when no record can hold its
/// rank: with the record unwritable, the next pass still keeps every runtime
/// copy's config.
#[test]
fn a_created_home_ranks_lowest_without_its_record() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    write_recorded_json(
        &r1,
        &json!({
            "numStartups": 3,
            "projects": {"/a": {"trust": true}},
            "mcpServers": {"mine": {"command": "x"}, "clauth": {"type": "stdio", "command": "clauth", "args": ["mcp"]}},
        }),
        t(20),
    );
    let _live_r1 = arm_live_copy(&r1);
    // The wire cannot persist a record here (a directory sits at the record
    // path); the copy's own earlier entry is what survives, restored after the
    // wire so the pass still knows clauth wrote it.
    let record = crate::claude_json::written_record_path().expect("record path");
    let saved_record = fs::read(&record).expect("read record");
    fs::remove_file(&record).expect("remove the record file");
    fs::create_dir_all(&record).expect("a directory where the record goes");

    wire_mcp_server().expect("wire");
    fs::remove_dir_all(&record).expect("remove the record directory");
    fs::write(&record, &saved_record).expect("restore the record");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    for (name, path) in [("r1", &r1), ("home", &global)] {
        let doc = read_doc(path);
        assert_eq!(doc["numStartups"], json!(3), "{name} keeps the counter");
        assert_eq!(
            doc["projects"],
            json!({"/a": {"trust": true}}),
            "{name} keeps the projects"
        );
        assert_eq!(
            doc["mcpServers"]["mine"],
            json!({"command": "x"}),
            "{name} keeps the user's own MCP server"
        );
    }
}

/// A home the wire creates whose stamp the filesystem refuses (the seam forces
/// it) answers `failed to write <home>` and leaves no home and no staged temp:
/// a failed stamp is a failed write, never a clamp.
#[test]
fn a_failed_home_create_stamp_refuses_the_wire_with_no_home_or_temp() {
    let _home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    assert!(!global.exists());

    crate::jsonsync::set_stamp_error_hook(true);
    let _guard = crate::testutil::StampErrorHookGuard;
    let err = wire_mcp_server().expect_err("a refused home-create stamp fails the wire");
    drop(_guard);

    assert_eq!(
        format!("{err:#}"),
        format!(
            "failed to write {}: test-forced stamp failure",
            global.display()
        ),
        "the wire names only the failed home write"
    );
    assert!(!global.exists(), "a refused stamp leaves no home");
    let temps = home_dir()
        .expect("home")
        .read_dir()
        .expect("read home")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.contains(".claude.json.tmp."))
        })
        .count();
    assert_eq!(temps, 0, "a refused stamp leaves no staged temp");
}

/// The home the wire creates is stamped at 1980-01-02T00:00:00Z: below every
/// Claude Code write, and inside the date range FAT and exFAT store.
#[test]
fn a_created_home_is_stamped_at_the_fat_floor() {
    // 3653 days from 1970-01-01 to 1980-01-02 (two leap days, 1972 and 1976).
    let floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(3_653 * 86_400);
    let _home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    assert!(!global.exists());

    wire_mcp_server().expect("wire");

    let stamped = fs::metadata(&global)
        .expect("the created home")
        .modified()
        .expect("mtime");
    assert_eq!(stamped, floor, "the created home's stamp");
}

/// A copy that moves once under the wire and then settles is re-read and
/// wired: one move is no refusal.
#[test]
fn a_copy_moving_once_mid_wire_is_still_wired() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(&global, &json!({"numStartups": 2}));
    write_json(&r1, &json!({"numStartups": 2}));
    let _live_r1 = arm_live_copy(&r1);

    let guard = after_wire_read({
        let r1 = r1.clone();
        move || {
            write_json(&r1, &json!({"numStartups": 3, "ccR1": true}));
            crate::testutil::set_mtime(&r1, std::time::SystemTime::now());
        }
    });
    let wired = wire_mcp_server();
    drop(guard);

    wired.expect("one move is re-read, not refused");
    let doc = read_doc(&r1);
    assert_eq!(doc["ccR1"], json!(true), "the moved copy keeps its write");
    assert_eq!(
        doc["mcpServers"]["clauth"]["command"],
        json!("clauth"),
        "the moved copy is wired"
    );
}

/// Wiring with no home `.claude.json` creates one, and the next pass keeps
/// every runtime copy's own config: projects, counters, the user's own MCP
/// servers and the account all survive, and every copy gains the wire's entry.
#[test]
fn wiring_with_no_home_never_empties_the_runtime_copies() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    let r2 = runtime_copy(home.home(), "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
    for (path, email) in [(&r1, "r1@x"), (&r2, "r2@x")] {
        write_json(
            path,
            &json!({
                "numStartups": 3,
                "projects": {"/a": {"trust": true}},
                "mcpServers": {"mine": {"command": "x"}},
                "oauthAccount": {"emailAddress": email},
            }),
        );
    }
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    crate::testutil::set_mtime(&r1, t(20));
    crate::testutil::set_mtime(&r2, t(20));
    let _live_r1 = arm_live_copy(&r1);
    let _live_r2 = arm_live_copy(&r2);

    wire_mcp_server().expect("wire");
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("next pass");

    for (name, path, email) in [
        ("r1", &r1, Some("r1@x")),
        ("r2", &r2, Some("r2@x")),
        ("home", &global, None),
    ] {
        let doc = read_doc(path);
        assert_eq!(doc["numStartups"], json!(3), "{name} keeps its counter");
        assert_eq!(
            doc["projects"],
            json!({"/a": {"trust": true}}),
            "{name} keeps its projects"
        );
        assert_eq!(
            doc["mcpServers"]["mine"],
            json!({"command": "x"}),
            "{name} keeps the user's own MCP server"
        );
        assert_eq!(
            doc["mcpServers"]["clauth"]["command"],
            json!("clauth"),
            "{name} gains the wire's entry"
        );
        if let Some(email) = email {
            assert_eq!(
                doc["oauthAccount"],
                json!({"emailAddress": email}),
                "{name} keeps its own account"
            );
        }
    }
}

/// A home the wire creates at the floor stamp still wakes the stat fast path on
/// the next ordinary pass: a changed member SET reads as a change, so home is
/// filled with the shared keys even though its floor mtime never advances the
/// newest mtime.
#[test]
fn a_wired_home_at_the_floor_wakes_the_fast_path() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    let r2 = runtime_copy(home.home(), "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    for (path, email) in [(&r1, "r1@x"), (&r2, "r2@x")] {
        write_recorded_json(
            path,
            &json!({
                "numStartups": 3,
                "projects": {"/a": {"trust": true}},
                "mcpServers": {"mine": {"command": "x"}, "clauth": {"type": "stdio", "command": "clauth", "args": ["mcp"]}},
                "oauthAccount": {"emailAddress": email},
            }),
            t(20),
        );
    }

    // Prime the stat fast path with no home: one pass converges the wired
    // copies and records their snapshot.
    crate::claude_json::reset_last_synced();
    crate::claude_json::sync_once().expect("prime the fast path");

    let _live_r1 = arm_live_copy(&r1);
    let _live_r2 = arm_live_copy(&r2);
    wire_mcp_server().expect("wire");
    // No reset: the next pass must still run — the floor-stamped home is a new
    // member, so the member SET changed even though no mtime moved forward.
    crate::claude_json::sync_once().expect("the fast path sees the new home");

    let g = read_doc(&global);
    assert_eq!(g["numStartups"], json!(3), "home gains the shared counter");
    assert_eq!(
        g["projects"],
        json!({"/a": {"trust": true}}),
        "home gains the shared projects"
    );
    assert_eq!(
        g["mcpServers"]["mine"],
        json!({"command": "x"}),
        "home gains the user's own MCP server"
    );
    assert_eq!(
        g["mcpServers"]["clauth"]["command"],
        json!("clauth"),
        "home keeps the wire's entry"
    );
}

/// The wire's held-lock refusal, pinned by equality: clauth's own sync holds
/// the lock, never Claude Code.
#[test]
fn wiring_refuses_a_held_sync_lock_with_the_lock_line() {
    let _home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    write_json(&global, &json!({"numStartups": 2}));

    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        let _lock = crate::claude_json::JsonSyncLock::acquire().expect("peer lock");
        held_tx.send(()).expect("send");
        let _ = done_rx.recv();
    });
    held_rx.recv().expect("peer holds the lock");
    let err = wire_mcp_server().expect_err("a held lock must refuse the wire");
    done_tx.send(()).expect("release the peer's lock");
    let _ = peer.join();
    assert_eq!(
        err.to_string(),
        "clauth is syncing Claude Code's config; try again in a moment",
        "the held-lock refusal names clauth's own sync"
    );
}

/// The wire's keep-changing refusal, pinned by equality: a runtime copy the
/// post-write hook keeps tearing out stays torn through the wire's bounded
/// retries, and the copies the wire already wrote keep their entry.
#[test]
fn wiring_refuses_a_copy_that_keeps_changing_with_the_retry_line() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    let r2 = runtime_copy(home.home(), "p2", "runtime-4242-1");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    fs::create_dir_all(r2.parent().expect("has parent")).expect("mkdir");
    write_json(&global, &json!({"numStartups": 2}));
    write_json(&r1, &json!({"numStartups": 3, "ccR1": true}));
    write_json(&r2, &json!({"numStartups": 1}));
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    crate::testutil::set_mtime(&global, t(10));
    crate::testutil::set_mtime(&r1, t(20));
    crate::testutil::set_mtime(&r2, t(5));
    let _live_r1 = arm_live_copy(&r1);
    let _live_r2 = arm_live_copy(&r2);

    let torn_r2 = r2.clone();
    crate::jsonsync::set_post_write_hook(Some(Box::new(move || {
        fs::write(&torn_r2, b"{\"numStartups\": 1, \"ccR1\": tr").expect("torn");
        crate::testutil::set_mtime(&torn_r2, std::time::SystemTime::now());
    })));
    let guard = PostWriteHookGuard;
    let err = wire_mcp_server().expect_err("a copy that keeps changing must refuse");
    drop(guard);
    assert_eq!(
        err.to_string(),
        format!(
            "{} is being written by Claude Code; try again in a moment",
            r2.display()
        ),
        "the retry refusal names the copy that kept changing"
    );
    // Home is always the first member the wire edits, so its write lands before
    // the refusal: partial progress stays written.
    assert_eq!(
        read_doc(&global)["mcpServers"]["clauth"]["command"],
        json!("clauth"),
        "home keeps the wire's entry from before the refusal"
    );
}

/// The wire's parse refusal, pinned by equality: a runtime copy that parses to
/// a non-object refuses before any member is written.
#[test]
fn wiring_refuses_a_non_object_runtime_copy_with_the_parse_line() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let r1 = runtime_copy(home.home(), "p1", "runtime-4242-0");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(&global, &json!({"numStartups": 2}));
    fs::write(&r1, b"[1, 2, 3]").expect("non-object copy");
    let _live_r1 = arm_live_copy(&r1);
    let before_global = fs::read(&global).expect("read");
    let before_r1 = fs::read(&r1).expect("read");

    let err = wire_mcp_server().expect_err("a non-object runtime copy must refuse");
    assert_eq!(
        err.to_string(),
        format!(
            "{} does not parse as a JSON object; nothing was changed",
            r1.display()
        ),
        "the parse refusal names the copy and promises nothing changed"
    );
    assert_eq!(
        fs::read(&global).expect("read"),
        before_global,
        "home is unchanged"
    );
    assert_eq!(
        fs::read(&r1).expect("read"),
        before_r1,
        "the runtime copy is unchanged"
    );
}

/// The wire's parse refusal, pinned by equality on an unreadable home: a
/// directory sitting where the home file goes refuses before any member is
/// written, and nothing is created or changed.
#[test]
fn wiring_refuses_an_unreadable_home_with_the_parse_line() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    fs::create_dir_all(&path).expect("a directory where the home file goes");

    let err = wire_mcp_server().expect_err("a directory home must refuse the wire");
    assert_eq!(
        err.to_string(),
        format!(
            "{} does not parse as a JSON object; nothing was changed",
            path.display()
        ),
        "the parse refusal names the home path"
    );
    assert!(path.is_dir(), "the directory home is left as it was");
    assert_eq!(
        fs::read_dir(&path).expect("read").count(),
        0,
        "nothing was written"
    );
}

/// The wire's parse refusal, pinned by equality on a torn home: a home caught
/// mid-write refuses with the parse line and the bytes are left exactly as they
/// were.
#[test]
fn wiring_refuses_a_torn_home_with_the_parse_line() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("home").join(".claude.json");
    let torn = b"{\"numStartups\": 3, \"projects\": {\"/a\": {\"x\"".to_vec();
    fs::write(&path, &torn).expect("write torn home");

    let err = wire_mcp_server().expect_err("a torn home must refuse the wire");
    assert_eq!(
        err.to_string(),
        format!(
            "{} does not parse as a JSON object; nothing was changed",
            path.display()
        ),
        "the parse refusal names the home path"
    );
    assert_eq!(
        fs::read(&path).expect("read"),
        torn,
        "the torn home is left exactly as it was"
    );
}

/// The wire walks the LIVE set: a dead tree's `.claude.json` is neither wired
/// nor recorded, so no record entry for a ghost's path is manufactured.
#[test]
fn a_dead_trees_copy_is_neither_wired_nor_recorded() {
    let home = HomeSandbox::new();
    let global = home_dir().expect("home").join(".claude.json");
    let dead = runtime_copy(home.home(), "p1", "runtime-4242-0");
    fs::create_dir_all(dead.parent().expect("has parent")).expect("mkdir");
    write_json(&global, &json!({"numStartups": 2}));
    write_json(&dead, &json!({"numStartups": 1}));
    let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + s);
    crate::testutil::set_mtime(&global, t(10));
    crate::testutil::set_mtime(&dead, t(20));
    // No live marker: the dead tree is invisible to the wire.
    let record = crate::claude_json::written_record_path().expect("record path");
    let dead_str = dead.to_string_lossy().into_owned();

    wire_mcp_server().expect("wire");

    assert_eq!(
        read_doc(&dead)["mcpServers"],
        Value::Null,
        "a dead tree's copy is not wired"
    );
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(&record).expect("read record")).expect("parse record");
    assert!(
        !entries
            .iter()
            .any(|e| e["path"].as_str() == Some(dead_str.as_str())),
        "the wire must not record a dead tree's copy"
    );
    assert_eq!(
        read_doc(&global)["mcpServers"]["clauth"]["command"],
        json!("clauth"),
        "home is still wired"
    );
}
