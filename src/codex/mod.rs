//! Codex (OpenAI CLI) live-credential mechanics — the codex-harness sibling of
//! `claude.rs` (CDX-1). Much simpler by design: no Keychain, no symlink, no
//! LinkState. The live file `~/.codex/auth.json` is compared by CONTENT
//! (account_id anchor + access-token fingerprint) against per-profile
//! snapshots at `~/.clauth/profiles/<name>/codex-auth.json`, and every
//! store/switch/adopt copies raw bytes whole-file (§0.3 raw round-trip — see
//! `auth.rs`). Lock discipline: every mutating helper acquires
//! `with_state_lock` itself (re-entrant, mirroring `claude.rs`), so callers
//! may hold it around larger compound operations.

pub(crate) mod auth;
pub(crate) mod login;
pub(crate) mod oauth;
pub(crate) mod poll;
pub(crate) mod usage;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::lock::with_state_lock;
use crate::profile::{atomic_write_600, home_dir, mkdir_700, profile_subpath};

pub(crate) use auth::CodexAuthFile;

pub(crate) fn codex_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".codex"))
}

/// The live codex credential file — whichever account codex is logged into.
pub(crate) fn live_auth_path() -> Result<PathBuf> {
    Ok(codex_dir()?.join("auth.json"))
}

/// A profile's stored codex chain. Distinct filename from the claude
/// `credentials.json` so a directory listing tells the harness at a glance.
pub(crate) fn profile_auth_path(name: &str) -> Result<PathBuf> {
    profile_subpath(name, "codex-auth.json")
}

/// Raw live auth.json bytes; `Ok(None)` when the file doesn't exist.
pub(crate) fn read_live() -> Result<Option<Vec<u8>>> {
    let path = live_auth_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Raw stored bytes for `name`; `Ok(None)` when the profile has no codex login.
pub(crate) fn read_profile_auth(name: &str) -> Result<Option<Vec<u8>>> {
    let path = profile_auth_path(name)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Install `bytes` as the live auth.json (atomic, 0600) — the switch's final
/// write. Codex's own guarded reload never adopts a different account, so a
/// running session errors at its next refresh boundary rather than clobbering
/// this (feasibility §2.3); the swap takes effect for NEW sessions.
pub(crate) fn write_live(bytes: &[u8]) -> Result<()> {
    with_state_lock(|| {
        atomic_write_600(&live_auth_path()?, bytes).context("failed to write ~/.codex/auth.json")
    })
}

/// Store `bytes` as `name`'s codex chain (atomic, 0600, whole-file).
pub(crate) fn write_profile_auth(name: &str, bytes: &[u8]) -> Result<()> {
    with_state_lock(|| {
        mkdir_700(&crate::profile::profile_dir(name)?)?;
        atomic_write_600(&profile_auth_path(name)?, bytes)
            .with_context(|| format!("failed to write {name}/codex-auth.json"))
    })
}

/// How codex stores CLI credentials (`cli_auth_credentials_store` in
/// `~/.codex/config.toml`). CDX-1 only supports the default `file` mode:
/// under `keyring`/`auto`/`ephemeral` the auth.json on disk is not the live
/// credential, so capture/switch must refuse rather than trade stale bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreMode {
    File,
    Other(String),
}

impl StoreMode {
    pub(crate) fn is_file(&self) -> bool {
        *self == StoreMode::File
    }
}

/// Lenient read: missing config.toml, missing key, or unparseable TOML all
/// mean `File` (the codex default — and an unparseable config means codex
/// itself won't run, so there is nothing fresher than the file anyway; doctor
/// surfaces the parse problem separately).
pub(crate) fn store_mode() -> StoreMode {
    let Ok(dir) = codex_dir() else {
        return StoreMode::File;
    };
    let Ok(content) = std::fs::read_to_string(dir.join("config.toml")) else {
        return StoreMode::File;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return StoreMode::File;
    };
    match value
        .get("cli_auth_credentials_store")
        .and_then(|v| v.as_str())
    {
        None | Some("file") => StoreMode::File,
        Some(other) => StoreMode::Other(other.to_string()),
    }
}

/// Best-effort: is any `codex` process running? A switch still proceeds — a
/// live session keeps its in-memory account until its next refresh boundary,
/// then errors without clobbering the swapped file (feasibility §2.3) — so
/// this only feeds a caller's warning line. Absent `pgrep` (non-unix) → false.
pub(crate) fn codex_processes_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "codex"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// RESCUE-2 sibling for codex: preserve the live auth.json before a switch
/// overwrites it with a login clauth doesn't own. Copies the raw bytes into
/// `~/.clauth/quarantine/<epoch-ms>-<seq>-<label>.codex-auth.json` (0600) so
/// the forced switch is loss-free. Retention and same-millisecond sequencing
/// mirror `claude::archive_live_credentials`; the `.codex-auth.json` suffix
/// keeps the two retentions independent.
pub(crate) fn archive_live_auth(label: &str) -> Result<PathBuf> {
    with_state_lock(|| {
        let path = live_auth_path()?;
        let bytes = std::fs::read(&path).context("live auth.json vanished before archive")?;
        let dir = crate::profile::clauth_dir()?.join("quarantine");
        std::fs::create_dir_all(&dir).context("failed to create quarantine dir")?;
        static ARCHIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = ARCHIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = dir.join(format!(
            "{}-{seq:04}-{label}.codex-auth.json",
            crate::usage::now_ms()
        ));
        atomic_write_600(&dest, bytes).context("failed to write quarantine copy")?;
        const QUARANTINE_KEEP: usize = 20;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut archived: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".codex-auth.json"))
                })
                .collect();
            archived.sort();
            if archived.len() > QUARANTINE_KEEP {
                for old in &archived[..archived.len() - QUARANTINE_KEEP] {
                    let _ = std::fs::remove_file(old);
                }
            }
        }
        Ok(dest)
    })
}

/// Which stored codex profile owns the live login, by account_id anchor.
/// `Ok(None)` when the live file is missing/unparseable/anchorless or matches
/// no candidate. `candidates` are (name, stored-bytes) pairs the caller
/// already read — keeping IO at the call site keeps this pure enough to test
/// exhaustively.
pub(crate) fn live_owner<'a>(
    live: &CodexAuthFile,
    candidates: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Option<String> {
    let live_id = live.account_id()?;
    for (name, bytes) in candidates {
        let Ok(stored) = CodexAuthFile::parse(bytes) else {
            continue;
        };
        if stored.account_id().as_deref() == Some(live_id.as_str()) {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
#[path = "../../tests/inline/codex.rs"]
mod tests;
