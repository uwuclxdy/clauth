//! Local-read probes backing the Plugin tab: binary-on-`PATH` resolution, Claude
//! Code's plugin registry (`installed_plugins.json` / `known_marketplaces.json`),
//! the manual `mcpServers` wiring, the `claude --version` string, and the one
//! safe write the tab performs (wire `mcpServers.clauth`).
//!
//! Everything here is a cheap filesystem/`PATH` read except [`cc_version`], which
//! runs one short subprocess; the Plugin tab caches that result. Nothing spawns a
//! background thread. All path reads route through the test-overridable
//! `home_dir()` / `claude_dir()`, so the inline tests can sandbox `$HOME`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::profile::{atomic_write, claude_dir, home_dir};

/// Plugin id in the registry (`<plugin>@<marketplace>`).
pub(crate) const PLUGIN_ID: &str = "clauth@clauth";
/// Marketplace key in `known_marketplaces.json`.
pub(crate) const MARKETPLACE_KEY: &str = "clauth";

/// Resolve `binary` against `PATH`, returning the first hit. The OS does this
/// implicitly when spawning, but a presence *check* needs it spelled out. On
/// Windows the usual executable extensions are tried too; on Unix the exec bit is
/// required so a non-executable file named `clauth` doesn't read as "resolved".
pub(crate) fn on_path(binary: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{binary}{ext}")))
            .find(|candidate| is_executable(candidate))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// One install record from `plugins["clauth@clauth"]`. Every field is optional —
/// CC's schema is treated leniently so a shape change degrades to "unknown"
/// rather than a parse error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InstallRecord {
    pub(crate) scope: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) git_commit_sha: Option<String>,
    pub(crate) installed_at: Option<String>,
    pub(crate) install_path: Option<String>,
    pub(crate) project_path: Option<String>,
}

/// Marketplace source for `clauth`, from `known_marketplaces.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MarketplaceInfo {
    pub(crate) repo: Option<String>,
    pub(crate) install_location: Option<String>,
}

/// Where a manual `mcpServers.clauth` entry lives, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpWiring {
    /// `~/.claude.json` (user-global) carries it.
    GlobalConfig,
    /// A project `./.mcp.json` carries it.
    ProjectFile,
    /// No manual wiring found.
    None,
}

/// Install records for the clauth plugin; empty when the registry is absent,
/// unreadable, or carries no entry (all read as "not installed").
pub(crate) fn installed_records() -> Vec<InstallRecord> {
    let Some(root) = read_json(plugins_dir().map(|dir| dir.join("installed_plugins.json"))) else {
        return Vec::new();
    };
    root.get("plugins")
        .and_then(|plugins| plugins.get(PLUGIN_ID))
        .and_then(Value::as_array)
        .map(|records| records.iter().map(install_record_from).collect())
        .unwrap_or_default()
}

/// Marketplace record for clauth, when the marketplace is known to CC.
pub(crate) fn marketplace_known() -> Option<MarketplaceInfo> {
    let root = read_json(plugins_dir().map(|d| d.join("known_marketplaces.json")))?;
    let entry = root.get(MARKETPLACE_KEY)?;
    Some(MarketplaceInfo {
        repo: entry
            .get("source")
            .and_then(|source| source.get("repo"))
            .and_then(Value::as_str)
            .map(str::to_string),
        install_location: entry
            .get("installLocation")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Manual `mcpServers.clauth` wiring, preferring the user-global config over a
/// project file (the global is the one the fix writes).
pub(crate) fn manual_mcp_wiring() -> McpWiring {
    if global_claude_json_path().is_some_and(|path| json_has_clauth_mcp(&path)) {
        McpWiring::GlobalConfig
    } else if json_has_clauth_mcp(Path::new(".mcp.json")) {
        McpWiring::ProjectFile
    } else {
        McpWiring::None
    }
}

/// Whether the user-global `mcpServers.clauth` entry matches the canonical stdio
/// entry clauth writes. `None` when no global manual entry exists (nothing to
/// validate — a plugin install or a project file is judged elsewhere). `Some(false)`
/// flags drift: a stale absolute `command` or `args` missing `mcp` reads as "wired"
/// but won't launch the current server, so the tab re-offers the canonical write.
pub(crate) fn global_entry_drifted() -> Option<bool> {
    let entry = read_json(global_claude_json_path()).and_then(|root| {
        root.get("mcpServers")
            .and_then(|servers| servers.get("clauth"))
            .cloned()
    })?;
    let canon = clauth_mcp_entry();
    // `type` is allowed to be absent (CC defaults stdio); only command + args are
    // load-bearing for the launch.
    let same =
        entry.get("command") == canon.get("command") && entry.get("args") == canon.get("args");
    Some(!same)
}

/// Verdict of a live `clauth mcp` discovery handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpProbe {
    /// Server answered `server/discover` with a result advertising its tools.
    Ok,
    /// Server couldn't be spawned or didn't answer a valid result (reason).
    Failed(String),
}

/// Spawn `clauth mcp`, send one JSON-RPC `server/discover`, and confirm the reply
/// advertises tools. Client-faithful: catches a `clauth` that resolves on PATH but is
/// too old to serve (no `mcp` subcommand, or no stateless protocol) or boots then dies. Heavier than the
/// other probes — the server runs `gc_stale_runtimes()` at startup — so the tab
/// gates it behind `r` only. Drains stdout on a thread so a chatty server can't
/// deadlock the pipe; 3s budget, then kill.
fn probe_command() -> Command {
    let mut cmd = Command::new("clauth");
    cmd.arg("mcp")
        .env(crate::mcp::MCP_PROBE_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

pub(crate) fn mcp_boots() -> McpProbe {
    let mut child = match probe_command().spawn() {
        Ok(child) => child,
        Err(e) => return McpProbe::Failed(format!("spawn failed: {e}")),
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return McpProbe::Failed("no stdio pipes".to_string());
    };

    let req = discover_frame();
    if writeln!(stdin, "{req}")
        .and_then(|()| stdin.flush())
        .is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return McpProbe::Failed("write failed".to_string());
    }

    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });

    let verdict = match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(line)) => parse_discover_reply(&line),
        Ok(Err(e)) => McpProbe::Failed(format!("read failed: {e}")),
        Err(_) => McpProbe::Failed("no reply within 3s".to_string()),
    };
    // EOF on stdin + kill ends the server; the reader unblocks once stdout closes.
    let _ = child.kill();
    let _ = child.wait();
    drop(stdin);
    let _ = reader.join();
    verdict
}

/// The stateless opener the probe sends. An `initialize` frame would keep
/// passing against a server no modern client can talk to, since rmcp answers
/// both eras; there is no handshake left, so the version and the client's
/// capabilities ride in `_meta` on the request itself.
fn discover_frame() -> Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "clauth-probe", "version": env!("CARGO_PKG_VERSION")
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
}

/// JSON-RPC "method not found", which is what a `clauth` predating the stateless
/// protocol answers `server/discover` with.
const METHOD_NOT_FOUND: i64 = -32601;

/// Classify the first stdout line of a `server/discover` handshake. A result
/// proves the server booted, and the `tools` capability inside it is what
/// decides whether a real client exposes any tools at all — a forced
/// `tools/list` answers either way, so it cannot stand in for this.
fn parse_discover_reply(line: &str) -> McpProbe {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return McpProbe::Failed("unparseable reply".to_string());
    };
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        return McpProbe::Failed(if code == Some(METHOD_NOT_FOUND) {
            "no `server/discover`: the clauth on PATH predates the stateless protocol".to_string()
        } else {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            format!("server rejected `server/discover`: {message}")
        });
    }
    let Some(result) = value.get("result") else {
        return McpProbe::Failed("no result in reply".to_string());
    };
    if result.pointer("/capabilities/tools").is_none() {
        return McpProbe::Failed("server advertises no tools capability".to_string());
    }
    McpProbe::Ok
}

/// `claude --version`, trimmed to its first line. `None` when the binary is
/// missing or the call fails — the row then reports "unknown".
pub(crate) fn cc_version() -> Option<String> {
    let output = crate::runtime::claude_command()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// `~/.claude.json` (user-global), the file the wire fix edits.
pub(crate) fn global_claude_json_path() -> Option<PathBuf> {
    Some(home_dir().ok()?.join(".claude.json"))
}

/// Write `mcpServers.clauth` into `~/.claude.json`, preserving every other field
/// (key order is kept via serde_json's `preserve_order`). The entry mirrors the
/// plugin manifest so a manual wire matches a plugin install. Creates the file
/// when absent.
pub(crate) fn wire_mcp_server() -> Result<()> {
    let path = home_dir()?.join(".claude.json");
    let mut root: Map<String, Value> = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    };
    let entry = clauth_mcp_entry();
    match root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(servers) => {
            servers.insert("clauth".to_string(), entry);
        }
        // `mcpServers` existed but wasn't an object — replace it with a fresh map.
        other => *other = Value::Object(Map::from_iter([("clauth".to_string(), entry)])),
    }
    atomic_write(&path, serde_json::to_vec_pretty(&Value::Object(root))?)?;
    Ok(())
}

/// The canonical stdio entry clauth registers (matches `plugin.json`).
fn clauth_mcp_entry() -> Value {
    serde_json::json!({ "type": "stdio", "command": "clauth", "args": ["mcp"] })
}

/// CC's user-scope plugin registry. `claude` resolves its config dir as
/// `$CLAUDE_CONFIG_DIR` when set and non-empty, else `~/.claude`, and the
/// registry lives under it — so the probe reads the same resolution the CLI
/// writes. That is what makes the install fix converge: agentgear drives the
/// real CLI, which honors the override, and the recomputed row must read the
/// registry the CLI just wrote. `profile::claude_dir()` stays the `~/.claude`
/// view the credentials link uses; the registry is not the link.
fn plugins_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => claude_dir().ok()?,
    };
    Some(base.join("plugins"))
}

/// Parse a JSON file into a `Value`, returning `None` on any missing/unreadable/
/// unparseable input (graceful — the registry files often don't exist).
fn read_json(path: Option<PathBuf>) -> Option<Value> {
    let bytes = std::fs::read(path?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn json_has_clauth_mcp(path: &Path) -> bool {
    read_json(Some(path.to_path_buf()))
        .and_then(|root| {
            root.get("mcpServers")
                .and_then(|servers| servers.get("clauth"))
                .cloned()
        })
        .is_some()
}

fn install_record_from(value: &Value) -> InstallRecord {
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    InstallRecord {
        scope: field("scope"),
        version: field("version"),
        git_commit_sha: field("gitCommitSha"),
        installed_at: field("installedAt"),
        install_path: field("installPath"),
        project_path: field("projectPath"),
    }
}

#[cfg(test)]
#[path = "../tests/inline/plugin_probe.rs"]
mod tests;
