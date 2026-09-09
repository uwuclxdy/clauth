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
