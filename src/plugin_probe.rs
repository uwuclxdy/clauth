//! Local-read probes backing the Services tab: binary-on-`PATH` resolution, Claude
//! Code's plugin registry (`installed_plugins.json` / `known_marketplaces.json`),
//! the manual `mcpServers` wiring, the `claude --version` string, and the one
//! safe write the tab performs (wire `mcpServers.clauth`).
//!
//! Everything here is a cheap filesystem/`PATH` read except [`cc_version`], which
//! runs one short subprocess; the Services tab caches that result. Nothing spawns a
//! background thread. All path reads route through the test-overridable
//! `home_dir()` / `claude_dir()`, so the inline tests can sandbox `$HOME`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::profile::home_dir;

/// Plugin id in the registry (`<plugin>@<marketplace>`).
pub(crate) const PLUGIN_ID: &str = "clauth@clauth";
/// Marketplace key in `known_marketplaces.json`.
pub(crate) const MARKETPLACE_KEY: &str = "clauth";

/// Resolve `binary` against `PATH`, returning the first hit. The OS does this
/// implicitly when spawning, but a presence *check* needs it spelled out. On
/// Windows the usual executable extensions are tried too; on Unix the exec bit is
/// required so a non-executable file named `clauth` doesn't read as "resolved".
/// Under `cfg(test)` the search skips every `PATH` entry not lexically under the
/// active `HomeSandbox`'s home, so a lookup here never answers with a binary the
/// operator's box has installed in a plain `PATH` dir.
pub(crate) fn on_path(binary: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let path = std::env::var_os("PATH")?;
    // A test build's only home is the sandbox: skip every PATH entry outside
    // it. `home_dir()` panics when no sandbox is held — the crate's one loud
    // guard for reaching for the home in a test build — so the `else` arm is
    // unreachable there and exists only to satisfy the `Result` type.
    #[cfg(test)]
    let Ok(home) = home_dir() else {
        return None;
    };
    std::env::split_paths(&path).find_map(|dir| {
        #[cfg(test)]
        if !dir.starts_with(&home) {
            return None;
        }
        exts.iter()
            .map(|ext| dir.join(format!("{binary}{ext}")))
            .find(|candidate| is_executable(candidate))
    })
}

#[cfg(unix)]
pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(crate) fn is_executable(path: &Path) -> bool {
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

/// Write `mcpServers.clauth` into `~/.claude.json` and every live shared runtime
/// copy, preserving every other field (key order is kept via serde_json's
/// `preserve_order`). The entry mirrors the plugin manifest so a manual wire
/// matches a plugin install. Creates a missing home holding the entry alone.
///
/// Under the sync lock the wire reads, stats and parses every member first: an
/// unreadable member, one that parses to a non-object, or a torn member of
/// either kind refuses with nothing written. Then each member that lacks the
/// exact entry is edited, re-statted immediately before its rename, and written
/// through the record writer at the rank its bytes already carried — so no
/// clauth write outranks a Claude Code write it does not carry. A created home
/// is stamped at the floor stamp ([`crate::jsonsync::FLOOR_STAMP_SECS`], below
/// every Claude Code write); a member that kept moving is re-read and redone at
/// most three times before the wire refuses with the retry copy; a runtime copy
/// that vanished mid-wire is skipped.
/// Copies already written stay written.
pub(crate) fn wire_mcp_server() -> Result<()> {
    let home_path = home_dir()?.join(".claude.json");
    let Some(_lock) = crate::claude_json::JsonSyncLock::try_once()? else {
        anyhow::bail!("clauth is syncing Claude Code's config; try again in a moment");
    };
    let members = crate::claude_json::live_known_paths()?;
    let record = crate::claude_json::written_record_path();

    // Phase 1: read, stat and parse every member before writing any, so the
    // parse refusal leaves nothing changed. A missing runtime copy is absent; a
    // missing home is created in phase 2.
    let mut snaps: Vec<WireMember> = Vec::new();
    for path in &members {
        let is_home = *path == home_path;
        match read_wire_member(path, is_home)? {
            WireRead::Object(member) => snaps.push(member),
            WireRead::Missing if is_home => snaps.push(WireMember::missing_home(path.clone())),
            WireRead::Missing => {}
        }
    }

    #[cfg(test)]
    run_wire_after_read_hook();

    // Phase 2: add the entry to every member that lacks it, re-checking each
    // member before its rename.
    for snap in snaps {
        if already_wired(&snap.obj) {
            continue;
        }
        write_wire_member_with_retry(snap, record.as_deref())?;
    }
    Ok(())
}

/// Test-only seam between the wire's read phase and its first write, where a
/// Claude Code write landing mid-wire becomes deterministic. Armed only while
/// the arming test holds a `HomeSandbox`, so `HOME_TEST_LOCK` serializes it.
#[cfg(test)]
static WIRE_AFTER_READ_HOOK: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> =
    std::sync::Mutex::new(None);

/// Arm or clear [`WIRE_AFTER_READ_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_wire_after_read_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *WIRE_AFTER_READ_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Run the armed [`WIRE_AFTER_READ_HOOK`] outside its slot lock.
#[cfg(test)]
fn run_wire_after_read_hook() {
    let hook = WIRE_AFTER_READ_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// The wire's read of one member.
enum WireRead {
    /// A readable JSON object member.
    Object(WireMember),
    /// The file is absent.
    Missing,
}

/// One member the wire edits: its path, its read stat (when it existed), its
/// raw bytes, and its parsed object.
struct WireMember {
    path: PathBuf,
    is_home: bool,
    /// `None` for a home created fresh (nothing to re-stat before its rename).
    stat: Option<(SystemTime, u64)>,
    /// The bytes this member was read from (empty for a fresh home), for
    /// [`crate::jsonsync::carried_rank_of`]'s digest.
    bytes: Vec<u8>,
    obj: Map<String, Value>,
}

impl WireMember {
    fn missing_home(path: PathBuf) -> Self {
        Self {
            path,
            is_home: true,
            stat: None,
            bytes: Vec::new(),
            obj: Map::new(),
        }
    }
}

/// Read, stat and parse one member. An unreadable member, one that parses to a
/// non-object, or a torn member of either kind refuses with the parse line.
fn read_wire_member(path: &Path, is_home: bool) -> Result<WireRead> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WireRead::Missing),
        Err(_) => return Err(parse_refusal(path)),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WireRead::Missing),
        Err(_) => return Err(parse_refusal(path)),
    };
    let mtime = meta.modified().map_err(|_| parse_refusal(path))?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(obj)) => Ok(WireRead::Object(WireMember {
            path: path.to_path_buf(),
            is_home,
            stat: Some((mtime, meta.len())),
            bytes,
            obj,
        })),
        Ok(_) => Err(parse_refusal(path)),
        Err(_) => Err(parse_refusal(path)),
    }
}

/// The parse-refusal line, named by the member path.
fn parse_refusal(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{} does not parse as a JSON object; nothing was changed",
        path.display()
    )
}

/// Re-read one member after it moved, for the wire's bounded retry. `None` when
/// the member cannot be re-read cleanly (torn, non-object, unreadable, or
/// vanished) — all read as "still changing" to the retry loop.
fn re_read_member(path: &Path, is_home: bool) -> Option<WireMember> {
    read_wire_member(path, is_home)
        .ok()
        .and_then(|read| match read {
            WireRead::Object(member) => Some(member),
            WireRead::Missing => None,
        })
}

/// Whether `obj` already carries the exact canonical `mcpServers.clauth` entry.
fn already_wired(obj: &Map<String, Value>) -> bool {
    let entry = clauth_mcp_entry();
    obj.get("mcpServers")
        .and_then(|servers| servers.get("clauth"))
        == Some(&entry)
}

/// Insert the canonical `mcpServers.clauth` entry, keeping every other byte of
/// meaning (the user's own `mcpServers`, `oauthAccount`, `projects`, all of it).
fn apply_entry(obj: &Map<String, Value>) -> Map<String, Value> {
    let mut root = obj.clone();
    match root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(servers) => {
            servers.insert("clauth".to_string(), clauth_mcp_entry());
        }
        other => {
            *other = Value::Object(Map::from_iter([("clauth".to_string(), clauth_mcp_entry())]))
        }
    }
    root
}

/// Write the entry into one member, re-stating it before the rename and
/// re-reading it up to three times when it moved, then refusing with the retry
/// line. A runtime copy that vanished mid-wire is absent, like phase 1 treats a
/// missing copy: skip it and wire the rest. A home the wire found missing is
/// re-checked before its create and re-read when it now exists, so a home
/// Claude Code created mid-wire is never overwritten.
fn write_wire_member_with_retry(mut snap: WireMember, record: Option<&Path>) -> Result<()> {
    let owner_only = !snap.is_home;
    for _attempt in 0..3 {
        let moved = match snap.stat {
            Some((mtime, size)) => {
                crate::jsonsync::member_changed_since_read(&snap.path, mtime, size)
            }
            None => snap.path.symlink_metadata().is_ok(),
        };
        if !moved {
            let edited = apply_entry(&snap.obj);
            let bytes = serde_json::to_vec_pretty(&Value::Object(edited))?;
            let rank = match (record, snap.stat) {
                (Some(record), Some((mtime, size))) => {
                    crate::jsonsync::carried_rank_of(record, &snap.path, mtime, size, &snap.bytes)
                }
                _ => (i64::MIN, 0),
            };
            // A home the wire created carries no Claude Code write: stamp it at
            // the floor stamp, below every Claude Code write, so it ranks
            // lowest with or without its record entry.
            let backdate = (snap.stat.is_none()).then_some(crate::jsonsync::floor_stamp());
            crate::jsonsync::write_copy_and_record(
                record, &snap.path, &bytes, rank, owner_only, backdate,
            )
            .with_context(|| format!("failed to write {}", snap.path.display()))?;
            return Ok(());
        }
        if !snap.is_home
            && matches!(
                snap.path.symlink_metadata(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            )
        {
            return Ok(());
        }
        if let Some(next) = re_read_member(&snap.path, snap.is_home) {
            snap = next;
        }
    }
    anyhow::bail!(
        "{} is being written by Claude Code; try again in a moment",
        snap.path.display()
    )
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
    Some(crate::which::session_config_dir_or_global()?.join("plugins"))
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
