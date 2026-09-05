//! What a modern MCP client sees when it opens `clauth mcp`, driven against the
//! real binary.
//!
//! A green build says nothing about the wire. The failures this pins are all
//! silent ones: a server that answers `tools/list` while advertising no tools
//! capability (the client then exposes none of them), a server that introduces
//! itself with the SDK's identity instead of its own, and cache hints a
//! conforming client needs to stop re-fetching. None of them is an error at any
//! layer, so only a real handshake catches them.
//!
//! Unix only, and not for lack of a Windows story: the child resolves its home
//! through `dirs`, which on Windows reads `FOLDERID_Profile` from the shell API
//! and no environment variable at all, so the run could not be pointed away
//! from the operator's real `~/.clauth`.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

const PROTOCOL: &str = "2026-07-28";

/// Everything herdr injects into a pane process that the server reads. The
/// child runs the real serve path, which resolves a herdr pane reporter from
/// exactly these, so a run started from a herdr pane would otherwise report
/// this test's agent state at the operator's live session.
const HERDR_VARS: [&str; 5] = [
    "HERDR_PANE_ID",
    "HERDR_SOCKET_PATH",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_ENV",
];

/// A path no herdr binary can resolve to, so the reporter's second gate fails
/// as well: cleared vars alone would still leave a `PATH` herdr reachable.
const NO_HERDR_BIN: &str = "/nonexistent/clauth-mcp-handshake-no-herdr";

/// The spawn every case in this file uses.
fn server_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clauth"));
    command
        .arg("mcp")
        .env("HOME", home)
        .env("CLAUTH_MCP_PROBE", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("HERDR_BIN_PATH", NO_HERDR_BIN);
    for var in HERDR_VARS {
        command.env_remove(var);
    }
    command
}

/// The `_meta` envelope every request carries now that there is no handshake to
/// hang it on. Both keys are required; a missing one is a malformed request.
fn client_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": { "name": "handshake-test", "version": "0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// Pipe `requests` into a fresh `clauth mcp` and return its replies keyed by id.
/// `HOME` is a sandbox: the server runs its startup GC and reads config from
/// there, and `CLAUTH_MCP_PROBE` keeps it off the live-session tally.
fn handshake(requests: &[Value]) -> HashMap<i64, Value> {
    let home = TempDir::new().unwrap();
    let mut child = server_command(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn clauth mcp");

    let mut stdin = child.stdin.take().expect("piped stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("write frame");
    }
    // EOF is what ends the stdio session; without it the server waits forever.
    drop(stdin);

    let output = child.wait_with_output().expect("wait");
    assert!(output.status.success(), "server exited {}", output.status);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");

    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            // Every stdout byte belongs to a JSON-RPC frame; a stray print here
            // corrupts the protocol for a real client.
            let value: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("non-frame line on stdout ({e}): {line}"));
            let id = value["id"].as_i64().expect("reply carries an id");
            (id, value)
        })
        .collect()
}

fn discover_and_list() -> HashMap<i64, Value> {
    handshake(&[
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": { "_meta": client_meta(PROTOCOL) }
        }),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": { "_meta": client_meta(PROTOCOL) }
        }),
    ])
}

/// The only case here that drives the serve path's herdr reporter. Every other
/// test in the crate builds its server in-process and carries no reporter, so
/// this spawn is the one that would reach a real pane, and the run's own
/// environment decides that. Assert the neutralization on the command rather
/// than the outcome: a reporter that stays silent proves nothing on a box that
/// runs no herdr at all.
#[test]
fn the_spawn_cannot_reach_a_live_herdr() {
    let home = TempDir::new().unwrap();
    let command = server_command(home.path());
    let env: HashMap<_, _> = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
        .collect();

    for var in HERDR_VARS {
        assert_eq!(
            env.get(OsStr::new(var)),
            Some(&None),
            "{var} must be cleared for the child, got: {:?}",
            env.get(OsStr::new(var))
        );
    }
    let bin = env
        .get(OsStr::new("HERDR_BIN_PATH"))
        .expect("HERDR_BIN_PATH is pinned, never inherited")
        .as_ref()
        .expect("pinned to a path, not cleared");
    assert!(
        !Path::new(bin).exists(),
        "HERDR_BIN_PATH must resolve to nothing, got: {bin:?}"
    );
}

#[test]
fn discover_advertises_the_stateless_revision_and_the_tools_capability() {
    let replies = discover_and_list();
    let result = &replies[&1]["result"];

    assert_eq!(result["resultType"].as_str(), Some("complete"));
    let versions: Vec<&str> = result["supportedVersions"]
        .as_array()
        .expect("supportedVersions")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Both eras, deliberately: a legacy client has no fall-forward path, so
    // narrowing this list to the stateless revision strands every Claude Code
    // that predates it.
    for expected in [PROTOCOL, "2025-11-25"] {
        assert!(
            versions.contains(&expected),
            "{expected} must be advertised, got: {versions:?}"
        );
    }
    // The one a forced `tools/list` cannot stand in for: without this key a
    // conforming client renders the instructions and exposes zero tools.
    assert!(
        result["capabilities"]["tools"].is_object(),
        "capabilities must carry tools, got: {}",
        result["capabilities"]
    );
    assert!(
        !result["instructions"].as_str().unwrap_or("").is_empty(),
        "instructions reach a modern client through discover, not initialize"
    );
}

/// rmcp's default `Implementation` reads its own build env, so a server that
/// never sets one introduces itself to every client as "rmcp".
#[test]
fn discover_names_clauth_as_the_server() {
    let replies = discover_and_list();
    let info = &replies[&1]["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];

    assert_eq!(info["name"].as_str(), Some(env!("CARGO_PKG_NAME")));
    assert_eq!(info["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
}

/// Both results are fixed for the process, and rmcp hands out a zero TTL, which
/// a conforming client reads as "already stale".
#[test]
fn both_cacheable_results_carry_a_usable_freshness_hint() {
    let replies = discover_and_list();

    let discover = &replies[&1]["result"];
    assert!(
        discover["ttlMs"].as_u64().unwrap_or(0) > 0,
        "discover ttlMs must be set, got: {}",
        discover["ttlMs"]
    );
    // The instructions block names the operator's profiles.
    assert_eq!(discover["cacheScope"].as_str(), Some("private"));

    let tools = &replies[&2]["result"];
    assert!(
        tools["ttlMs"].as_u64().unwrap_or(0) > 0,
        "tools/list ttlMs must be set, got: {}",
        tools["ttlMs"]
    );
    assert_eq!(tools["cacheScope"].as_str(), Some("public"));
}

#[test]
fn tools_list_returns_the_whole_tool_surface() {
    let replies = discover_and_list();
    let result = &replies[&2]["result"];

    assert_eq!(result["resultType"].as_str(), Some("complete"));
    let mut names: Vec<&str> = result["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    names.sort_unstable();
    // Four tools, the slice-1 surface: `delegate` keeps its name because the
    // bundled PostToolUse hook matcher is anchored `delegate$` — a rename there
    // silently breaks result auto-delivery.
    assert_eq!(names, ["delegate", "monitor", "profiles", "switch_profile"]);
    for tool in result["tools"].as_array().expect("tools") {
        assert!(
            tool["inputSchema"].is_object(),
            "{} must ship an input schema",
            tool["name"]
        );
    }
}

/// There is no handshake left to require: a client may open on the request it
/// actually wants, and `server/discover` is optional.
#[test]
fn a_modern_client_may_open_on_any_request() {
    let replies = handshake(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": client_meta(PROTOCOL) }
    })]);

    assert_eq!(
        replies[&1]["result"]["resultType"].as_str(),
        Some("complete")
    );
}

/// The two malformed requests a real client can produce. Both must be errors,
/// and the version one must name what the server does support or the client has
/// nothing to retry with.
///
/// They ride behind a valid opener deliberately. As the very first frame, a
/// request carrying no `_meta` at all never establishes an era, and rmcp ends
/// the stdio session instead of answering it.
#[test]
fn a_malformed_request_is_refused_with_the_spec_codes() {
    let replies = handshake(&[
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": { "_meta": client_meta(PROTOCOL) }
        }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list",
            "params": { "_meta": client_meta("1900-01-01") }
        }),
    ]);

    assert_eq!(replies[&2]["error"]["code"].as_i64(), Some(-32602));

    let unsupported = &replies[&3]["error"];
    assert_eq!(unsupported["code"].as_i64(), Some(-32022));
    let supported: Vec<&str> = unsupported["data"]["supported"]
        .as_array()
        .expect("data.supported")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        supported.contains(&PROTOCOL),
        "the retry list must name the stateless revision, got: {supported:?}"
    );
}

/// The other half of the dual-era posture: a legacy opener still yields a usable
/// session, carrying clauth's identity and the old result shape.
#[test]
fn a_legacy_initialize_still_negotiates() {
    let replies = handshake(&[
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "handshake-test", "version": "0" }
            }
        }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    ]);
    let result = &replies[&1]["result"];

    assert_eq!(result["protocolVersion"].as_str(), Some("2025-11-25"));
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(
        result["serverInfo"]["name"].as_str(),
        Some(env!("CARGO_PKG_NAME"))
    );

    // The old wire shape carries none of the 2026-07-28 fields. A legacy client
    // parsing strictly rejects a result that ships them.
    let listed = &replies[&2]["result"];
    assert!(
        !listed["tools"].as_array().expect("tools").is_empty(),
        "a legacy session must still reach the tools"
    );
    for field in ["resultType", "ttlMs", "cacheScope"] {
        assert!(
            listed.get(field).is_none(),
            "a legacy peer must not receive {field}, got: {}",
            listed[field]
        );
    }
}
