//! The Services → herdr `held question dot` toggle: herdr's Claude detection
//! override carrying clauth's `clauth_held_ask` rule.
//!
//! While the clauth mod holds a question, Claude Code keeps its busy spinner
//! title and herdr reads the pane `working`; a rule outranking the title that
//! matches the mod band's second line reads it `blocked`
//! (`docs/domain-knowledge.md`, herdr pane agent state). herdr reads one
//! override per agent, `<config root>/agent-detection/claude.toml`, and that
//! file shadows every remote manifest herdr fetches later, so:
//!
//! - with no override, clauth writes its own file: herdr's cached remote
//!   manifest, then clauth's block; the daemon's tick and the Services tab
//!   rebuild it when that cache's `version` moves;
//! - over a user's override, clauth appends only its marker-delimited block,
//!   and turning the toggle off removes exactly those bytes;
//! - an override already holding a rule with clauth's id is left alone.
//!
//! Every write is checked by herdr's own loader first (`agent explain --file`
//! over a scratch config root), applied with `herdr server
//! reload-agent-manifests` on every running server, and checked again in that
//! reload's own answer: an override herdr did not load is put back. What
//! clauth last wrote lives in `~/.clauth/herdr-detection.json`, never in
//! herdr's files.

use std::fs::{File, OpenOptions};
use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::logline::logline;

/// The override's place under herdr's config root
/// (`src/detect/manifest.rs` `override_path` at herdr v0.9.5).
const OVERRIDE_REL: [&str; 2] = ["agent-detection", "claude.toml"];
/// What clauth last wrote, under `~/.clauth`.
const RECORD_FILE: &str = "herdr-detection.json";
/// Serializes every pass across the TUI and the daemon.
const LOCK_FILE: &str = "herdr-detection.lock";
/// A pass holds the lock through one reload per running server, each bounded
/// by [`crate::herdr::PROBE_TIMEOUT`]; a pass waiting behind another waits
/// this long before it gives up, and the next tick or refresh retries. A test
/// build waits a moment: a test holds the lock to prove a pass takes it.
#[cfg(not(test))]
const LOCK_WAIT: Duration = Duration::from_secs(15);
#[cfg(test)]
const LOCK_WAIT: Duration = Duration::from_millis(200);

/// The first line of clauth's own override.
const FILE_HEADER: &str = "# Written by clauth for the Services -> herdr `held question dot` toggle: \
herdr's cached remote Claude manifest, then clauth's rule. clauth rebuilds it when herdr caches a \
newer manifest and deletes it when the toggle goes off, unless you edit it.\n";
const BLOCK_BEGIN: &str =
    "# >>> clauth held question dot: clauth removes this block when the toggle goes off";
const BLOCK_END: &str = "# <<< clauth held question dot";
/// The rule's id, which a user's own copy of the rule carries too.
pub(crate) const RULE_ID: &str = "clauth_held_ask";
/// The rule the probe measured (`docs/domain-knowledge.md`, the
/// `clauth_held_ask` bullet): it outranks `osc_title_working` (1100), reads
/// only the line right above the prompt box's top border, and matches the
/// band's whole second line in both shapes `plugins/hooks/register.ts` draws.
const RULE: &str = r#"[[rules]]
id = "clauth_held_ask"
state = "blocked"
priority = 1150
region = "last_non_empty_above_prompt_box"
visible_blocker = true
line_regex = ['^\s*(?:Allow this\? Answer here or on your phone\.\s+1: Allow\s+2: Deny|Answer on your phone, or here\.\s+1: Answer here\s+Esc cancels)\s*$']
"#;

/// The bytes clauth appends: a newline that ends whatever line came before,
/// then the marked rule. Removing exactly these bytes restores the file.
pub(crate) fn block() -> String {
    format!("\n{BLOCK_BEGIN}\n{RULE}{BLOCK_END}\n")
}

/// The card's failure lines, under the detection row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Failure {
    /// herdr did not load the written override, so clauth put the previous
    /// bytes back (or refused the write when herdr's loader rejected it).
    LoadFailed,
    /// A running herdr's reload did not answer; the file stays.
    NoReload,
}

/// What the override holds on clauth's account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Held {
    /// clauth's own file: the cached remote manifest at `version`, then
    /// `block`; `digest` is the sha256 of the whole file as written.
    File {
        version: String,
        digest: String,
        block: String,
    },
    /// `block` appended to the user's override.
    Block { block: String },
    /// The user's override already holds a rule with [`RULE_ID`].
    HandKept,
    /// Nothing written: no override and no readable cache, or a write herdr
    /// refused.
    Unapplied,
}

/// `~/.clauth/herdr-detection.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The override this record speaks for; `None` when herdr's config root
    /// could not be resolved.
    pub(crate) override_path: Option<PathBuf>,
    pub(crate) held: Held,
    /// The cached remote version the last pass read where `due` compares it
    /// (`Held::File`, `Held::Unapplied`, and a refused record of any held
    /// state), so a cache that has not moved since starts no pass.
    pub(crate) seen: Option<String>,
    pub(crate) failure: Option<Failure>,
    /// The block a refresh tried and herdr did not take, so the same write is
    /// not retried on every cadence until the band copy or the cache moves.
    #[serde(default)]
    pub(crate) attempted: Option<String>,
    /// What clauth refused to touch or could not write: the digest of an
    /// override holding more than one clauth block, the digest or marker a
    /// failed write left, or the target a link clauth could not follow. The
    /// due check and the watcher read the file back, so no later check starts
    /// a pass until the file reads differently or becomes readable, the
    /// cached version moves or the toggle flips.
    #[serde(default)]
    pub(crate) refused: Option<String>,
}

/// herdr's cached remote Claude manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cache {
    pub(crate) bytes: Vec<u8>,
    pub(crate) version: String,
}

/// Where one pass reads and writes.
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    /// The herdr binary every subprocess runs.
    pub(crate) bin: PathBuf,
    /// `~/.clauth`: the record, the lock and the loader check's scratch.
    pub(crate) clauth_dir: PathBuf,
}

impl Ctx {
    fn record_path(&self) -> PathBuf {
        self.clauth_dir.join(RECORD_FILE)
    }
}

/// herdr's app dir name of a release build (`app_dir_name` in its
/// `src/config/io.rs` at v0.9.5).
const APP_DIR: &str = "herdr";

/// herdr's app dir name, read off its own config root: the root's last
/// component is `app_dir_name()`, which `config_dir` and `state_dir` both
/// join, so it names the override's and the cache's dir for the build that
/// answered.
fn app_dir(root: &Path) -> Option<String> {
    root.file_name()?.to_str().map(str::to_string)
}

/// The app dir name of an override path, `<root>/agent-detection/claude.toml`.
fn app_dir_of_override(path: &Path) -> Option<String> {
    app_dir(path.parent()?.parent()?)
}

/// herdr's state dir, the non-Windows arm of `state_dir` in herdr's
/// `src/config/io.rs` at v0.9.5: `$XDG_STATE_HOME/<app>`, else
/// `$HOME/.local/state/<app>`, else `<temp>/<app>-state`. herdr reads both
/// variables as UTF-8 and joins whatever they hold; a value that leaves the
/// path relative names a directory relative to herdr's own working
/// directory, which clauth cannot know, so it resolves to nothing.
fn state_dir_from(
    xdg_state_home: Option<String>,
    home: Option<String>,
    app: &str,
) -> Option<PathBuf> {
    let dir = match (xdg_state_home, home) {
        (Some(xdg), _) => PathBuf::from(xdg).join(app),
        (None, Some(home)) => PathBuf::from(home).join(".local/state").join(app),
        (None, None) => std::env::temp_dir().join(format!("{app}-state")),
    };
    dir.is_absolute().then_some(dir)
}

/// `<state>/agent-detection/remote/claude.toml`
/// (`src/detect/manifest_update.rs` `remote_manifest_path` at v0.9.5).
fn remote_manifest_path(app: &str) -> Option<PathBuf> {
    let state = state_dir_from(std::env::var("XDG_STATE_HOME").ok(), env_home(), app)?;
    #[cfg(test)]
    assert!(
        crate::profile::home_dir().is_ok_and(|home| state.starts_with(home)),
        "herdr's state dir resolved to {} outside the test's sandbox home",
        state.display()
    );
    Some(
        state
            .join("agent-detection")
            .join("remote")
            .join("claude.toml"),
    )
}

/// `$HOME` as herdr reads it; a test build reads the sandbox's home instead,
/// which panics when no sandbox is held.
fn env_home() -> Option<String> {
    #[cfg(test)]
    {
        crate::profile::home_dir()
            .ok()
            .map(|home| home.to_string_lossy().into_owned())
    }
    #[cfg(not(test))]
    {
        std::env::var("HOME").ok()
    }
}

/// herdr's `ManifestVersion::parse`: trimmed, dotted, every segment ASCII
/// digits fitting a `u64`.
fn manifest_version(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let valid = trimmed.split('.').all(|segment| {
        segment.chars().all(|ch| ch.is_ascii_digit()) && segment.parse::<u64>().is_ok()
    });
    valid.then(|| trimmed.to_string())
}

/// The manifest bytes as a table, when they describe Claude: the `id` (or an
/// alias) names claude. Anything else is no manifest herdr would take for it.
fn claude_table(bytes: &[u8]) -> Option<toml::Table> {
    let value: toml::Table = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    let claude = value.get("id").and_then(toml::Value::as_str) == Some("claude")
        || value
            .get("aliases")
            .and_then(toml::Value::as_array)
            .is_some_and(|aliases| aliases.iter().any(|a| a.as_str() == Some("claude")));
    claude.then_some(value)
}

/// Byte cap for herdr's cached remote manifest: a manifest is a few KB, and a
/// path under herdr's state dir must not balloon the tick thread.
const MAX_CACHE_BYTES: u64 = 1 << 20;

/// The cache as herdr would take it for Claude: claude's manifest whose
/// `version` herdr parses. A path that is not a regular file, or a file over
/// [`MAX_CACHE_BYTES`], reads as no cache. Its own loader judges the rest
/// before any write.
fn read_cache(path: &Path) -> Option<Cache> {
    let text = crate::profile::read_capped_file(path, MAX_CACHE_BYTES).ok()?;
    let value = claude_table(text.as_bytes())?;
    let version = manifest_version(value.get("version")?.as_str()?)?;
    Some(Cache {
        bytes: text.into_bytes(),
        version,
    })
}

fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// clauth's own file over `cache`.
fn file_bytes(cache: &[u8], block: &str) -> Vec<u8> {
    let mut bytes = FILE_HEADER.as_bytes().to_vec();
    bytes.extend_from_slice(cache);
    bytes.extend_from_slice(block.as_bytes());
    bytes
}

/// Whether `existing` parses and holds a `[[rules]]` entry whose `id` is
/// [`RULE_ID`]. A file that does not parse holds none herdr would run.
fn has_rule_id(existing: &[u8]) -> bool {
    let Some(table) = std::str::from_utf8(existing)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
    else {
        return false;
    };
    table
        .get("rules")
        .and_then(toml::Value::as_array)
        .is_some_and(|rules| {
            rules
                .iter()
                .any(|rule| rule.get("id").and_then(toml::Value::as_str) == Some(RULE_ID))
        })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `existing` with the first `needle` replaced by `with`.
fn replaced(existing: &[u8], at: usize, needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = existing.get(..at).unwrap_or_default().to_vec();
    out.extend_from_slice(with);
    out.extend_from_slice(existing.get(at + needle.len()..).unwrap_or_default());
    out
}

/// The blocks this record's held state says clauth appended, then the current
/// one: the record's survives a band copy change between a toggle's on and
/// off.
fn known_blocks(held: Option<&Held>) -> Vec<String> {
    let mut blocks = Vec::new();
    if let Some(Held::File { block, .. } | Held::Block { block }) = held {
        blocks.push(block.clone());
    }
    let current = block();
    if !blocks.contains(&current) {
        blocks.push(current);
    }
    blocks
}

/// Every place one of clauth's blocks starts in `existing`, in file order.
fn block_starts(existing: &[u8], held: Option<&Held>) -> Vec<(usize, String)> {
    let mut starts = Vec::new();
    for known in known_blocks(held) {
        let mut from = 0;
        while let Some(at) = find(&existing[from..], known.as_bytes()) {
            starts.push((from + at, known.clone()));
            from += at + 1;
        }
    }
    starts.sort_by_key(|(at, _)| *at);
    starts
}

/// Whether `existing` is clauth's own file as clauth last wrote it: only a
/// record's `File` digest says so, so an edited file stays the user's. With
/// no record nothing is known to be clauth's, and every file is the user's.
/// A link at the override path is the user's whatever bytes it reaches:
/// clauth deletes or rebuilds only a regular file.
fn owned(existing: &[u8], held: Option<&Held>, linked: bool) -> bool {
    if linked {
        return false;
    }
    match held {
        Some(Held::File { digest, .. }) => *digest == sha_hex(existing),
        _ => false,
    }
}

/// What a pass with the toggle on does to the override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OnPlan {
    /// Write these bytes; on success the override holds `held`. `user_tail`
    /// is set when the write moves bytes the user wrote after clauth's
    /// block: those go through a temp and a rename, never an in-place
    /// rewrite whose truncate a death could land in.
    Write {
        bytes: Vec<u8>,
        held: Held,
        user_tail: bool,
    },
    /// Leave the override as it is; it holds `held`.
    Keep(Held),
}

/// The toggle's on side, pure. A regular file clauth wrote and nobody edited
/// is rebuilt from the cache; any other file (a link at the override path
/// included) is the user's: its intact clauth block stays (refreshed when the
/// band copy moved), a hand-kept rule is left alone, and otherwise one block
/// is appended. A file that is not clauth's own and holds more than one
/// clauth block is a hand-duplicated shape clauth cannot undo exactly, so the
/// pass refuses it; ownership is decided first, so clauth's own file is
/// rebuilt whatever it holds.
pub(crate) fn plan_on(
    existing: Option<&[u8]>,
    linked: bool,
    held: Option<&Held>,
    cache: Option<&Cache>,
) -> Result<OnPlan> {
    let current = block();
    let Some(existing) = existing else {
        return Ok(match cache {
            Some(cache) => {
                let bytes = file_bytes(&cache.bytes, &current);
                OnPlan::Write {
                    held: Held::File {
                        version: cache.version.clone(),
                        digest: sha_hex(&bytes),
                        block: current,
                    },
                    bytes,
                    user_tail: false,
                }
            }
            None => OnPlan::Keep(Held::Unapplied),
        });
    };
    if owned(existing, held, linked) {
        let Some(cache) = cache else {
            return Ok(OnPlan::Keep(held.cloned().unwrap_or(Held::Unapplied)));
        };
        let bytes = file_bytes(&cache.bytes, &current);
        let held = Held::File {
            version: cache.version.clone(),
            digest: sha_hex(&bytes),
            block: current,
        };
        return Ok(if bytes == existing {
            OnPlan::Keep(held)
        } else {
            OnPlan::Write {
                bytes,
                held,
                user_tail: false,
            }
        });
    }
    let blocks = block_starts(existing, held);
    if blocks.len() > 1 {
        bail!("the override holds more than one clauth block; clauth did not touch it");
    }
    if let Some((at, known)) = blocks.into_iter().next() {
        let held = Held::Block {
            block: current.clone(),
        };
        return Ok(if known == current {
            OnPlan::Keep(held)
        } else {
            OnPlan::Write {
                bytes: replaced(existing, at, known.as_bytes(), current.as_bytes()),
                held,
                user_tail: at + known.len() < existing.len(),
            }
        });
    }
    if has_rule_id(existing) {
        return Ok(OnPlan::Keep(Held::HandKept));
    }
    let mut bytes = existing.to_vec();
    bytes.extend_from_slice(current.as_bytes());
    Ok(OnPlan::Write {
        bytes,
        held: Held::Block { block: current },
        user_tail: false,
    })
}

/// What a pass with the toggle off does to the override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffPlan {
    Delete,
    /// Write these bytes; `user_tail` is set when the write moves bytes the
    /// user wrote after clauth's block (the rename path, as on the on side).
    Write {
        bytes: Vec<u8>,
        user_tail: bool,
    },
    Keep,
}

/// The toggle's off side, pure: clauth's own unedited regular file goes; any
/// other file (a link at the override path included) loses exactly clauth's
/// block while that block is byte-intact, and is otherwise left as it is. A
/// file that is not clauth's own and holds more than one clauth block cannot
/// be restored exactly, so the pass refuses it; ownership is decided first,
/// so clauth's own file goes whatever it holds. `held` is `None` when the
/// record is gone: nothing is then known to be clauth's file, so only an
/// intact block comes out.
pub(crate) fn plan_off(
    existing: Option<&[u8]>,
    linked: bool,
    held: Option<&Held>,
) -> Result<OffPlan> {
    let Some(existing) = existing else {
        return Ok(OffPlan::Keep);
    };
    if owned(existing, held, linked) {
        return Ok(OffPlan::Delete);
    }
    let blocks = block_starts(existing, held);
    if blocks.len() > 1 {
        bail!("the override holds more than one clauth block; clauth did not touch it");
    }
    Ok(blocks
        .into_iter()
        .next()
        .map_or(OffPlan::Keep, |(at, known)| OffPlan::Write {
            bytes: replaced(existing, at, known.as_bytes(), b""),
            user_tail: at + known.len() < existing.len(),
        }))
}

/// Whether the toggle's state calls for a pass: off with a record left to
/// undo, unless a pass refused it; on with no record, with a block older than
/// this build's, or with clauth's file (or nothing applied) while the cache's
/// version moved since the last pass. A write herdr already refused is not
/// retried until this build's block changes, and a refused override not until
/// the cached version moves (the watcher's `refused_file_changed` reads the
/// file back for the rest of that state). `cache_version` runs only where the
/// cache decides, so a pass that is not due reads nothing of herdr's.
pub(crate) fn due(
    on: bool,
    record: Option<&Record>,
    cache_version: impl FnOnce() -> Option<String>,
) -> bool {
    let Some(record) = record else {
        return on;
    };
    if !on {
        return record.refused.is_none();
    }
    if record.refused.is_some() {
        return cache_version() != record.seen;
    }
    let current = block();
    let not_attempted = record.attempted.as_deref() != Some(current.as_str());
    match &record.held {
        Held::File { block: held, .. } => {
            (not_attempted && *held != current) || cache_version() != record.seen
        }
        Held::Block { block: held } => not_attempted && *held != current,
        Held::Unapplied => {
            (not_attempted && record.attempted.is_some()) || cache_version() != record.seen
        }
        Held::HandKept => false,
    }
}

/// The detection row and its notes, while the toggle is on: `key: value`
/// detail lines, the notes two-space indented. A refusal that left nothing
/// applied has no row: clauth wrote nothing, and the cards' words all suppose
/// a file it touched.
pub(crate) fn card_lines(record: &Record) -> Vec<String> {
    if record.refused.is_some() && record.held == Held::Unapplied {
        return Vec::new();
    }
    let mut lines = match &record.held {
        Held::File { version, .. } => {
            vec![format!("detection: herdr manifest {version} + clauth rule")]
        }
        Held::Block { .. } => vec![
            "detection: your override + clauth rule".to_string(),
            "  your override hides herdr's manifest updates".to_string(),
        ],
        Held::HandKept => vec![
            "detection: your override, which already has the rule".to_string(),
            "  a hand-kept rule won't follow clauth's band changes".to_string(),
        ],
        // Nothing applied yet: herdr has no cached remote Claude manifest to
        // build clauth's file from. The next pass with a cache applies it.
        Held::Unapplied => {
            vec!["detection: waiting for herdr to download its claude manifest".to_string()]
        }
    };
    lines.extend(record.failure.map(|failure| {
        match failure {
            Failure::LoadFailed => {
                "  herdr couldn't load the override, so clauth put the previous one back"
            }
            Failure::NoReload => "  herdr didn't reload, so the change applies at its next start",
        }
        .to_string()
    }));
    lines
}

/// The card for the toggle's current state: nothing while it is off.
pub(crate) fn card(on: bool) -> Vec<String> {
    if !on {
        return Vec::new();
    }
    crate::profile::clauth_dir()
        .ok()
        .and_then(|dir| load_record(&dir.join(RECORD_FILE)))
        .map_or_else(Vec::new, |record| card_lines(&record))
}

fn load_record(path: &Path) -> Option<Record> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_record(path: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record)?;
    crate::profile::atomic_write_600(path, bytes)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Save the record a failed pass leaves: the state it was about to change,
/// the cache version it read, and the refusal that keeps the next check from
/// retrying until the file moves.
fn save_refusal(
    record_path: &Path,
    override_path: Option<&Path>,
    held: &Held,
    seen: Option<String>,
    refused: String,
) -> Result<()> {
    save_record(
        record_path,
        &Record {
            override_path: override_path.map(Path::to_path_buf),
            held: held.clone(),
            seen,
            failure: None,
            attempted: None,
            refused: Some(refused),
        },
    )
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// The bytes of the regular file at `path`: a directory, FIFO, socket or
/// device is refused before the open, and the opened handle's own metadata
/// closes the window between the two calls, so a path a user or herdr
/// controls cannot block a check that only meant to read it. An absent path
/// keeps its `NotFound`, which callers read as a change.
fn read_regular(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The override as a pass finds it: the regular file a write acts on, whether
/// the user keeps a link at the override path, and its bytes.
struct OverrideFile {
    file: PathBuf,
    linked: bool,
    bytes: Option<Vec<u8>>,
}

/// Read the override at `path` through the file a write would act on: a link
/// at `path` is the user's, so its target's bytes are read and the target is
/// what a later write or removal names.
fn read_override(path: &Path) -> Result<OverrideFile> {
    let target = crate::canonical::write_target(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let bytes = read_optional(&target.file)?;
    Ok(OverrideFile {
        file: target.file,
        linked: target.linked,
        bytes,
    })
}

/// The file may have changed since the pass read it (the loader check runs a
/// herdr subprocess in that window); a save that landed there is the user's,
/// so it is never written over.
fn unchanged_since_read(path: &Path, old: Option<&[u8]>) -> Result<()> {
    match (read_regular(path), old) {
        (Ok(current), Some(old)) if current == old => Ok(()),
        (Ok(_), Some(_)) => bail!(
            "{} changed since the pass read it; clauth left it alone",
            path.display()
        ),
        (Ok(_), None) => bail!(
            "{} appeared since the pass read it; clauth left it alone",
            path.display()
        ),
        (Err(e), _) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        (Err(e), _) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// The bytes [`edit_in_place`] last wrote, so its in-place property is
/// observable: a whole-file rewrite would write everything.
#[cfg(test)]
pub(crate) static LAST_IN_PLACE_WRITTEN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);

/// The mode [`write_atomic`]'s temp was created with, so its birth mode is
/// observable: a temp born at the umask would be more permissive than the
/// target it replaces.
#[cfg(all(test, unix))]
pub(crate) static LAST_TEMP_MODE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(u32::MAX);

/// Turn a user's file at `path` from `old` into `new` in place where every
/// byte after the two files' common prefix is clauth's own block: an append
/// writes the appended bytes alone, a trailing block's removal only
/// truncates, and a trailing block's refresh rewrites the block's own tail.
/// The user's bytes are never rewritten and the file keeps its inode and
/// mode, so a death here can only leave a torn block, never a lost user byte.
/// A change that moves the user's own bytes takes [`write_atomic`] instead.
fn edit_in_place(path: &Path, old: &[u8], new: &[u8]) -> Result<()> {
    let keep = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let at = u64::try_from(keep).context("file offset past u64")?;
    #[cfg(test)]
    LAST_IN_PLACE_WRITTEN.store(new.len() - keep, Ordering::Relaxed);
    unchanged_since_read(path, Some(old))?;
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.set_len(at)
        .and_then(|()| file.seek(SeekFrom::Start(at)).map(drop))
        .and_then(|()| file.write_all(new.get(keep..).unwrap_or_default()))
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Write `new` beside `path` and move it into place: a death at any point
/// leaves the prior bytes or the new ones, never a torn file. An existing
/// target keeps its mode, and the temp is born at that mode rather than the
/// umask; a new one is created at the umask, the way `clauth herdr install`
/// writes herdr's `config.toml`.
fn write_atomic(path: &Path, old: Option<&[u8]>, new: &[u8]) -> Result<()> {
    unchanged_since_read(path, old)?;
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let temp = dir.join(format!(".{name}.clauth-tmp"));
    let target_mode = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if let Some(mode) = &target_mode {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        options.mode(mode.mode());
    }
    let result = (|| -> Result<()> {
        let mut file = options
            .open(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        #[cfg(all(test, unix))]
        if let Ok(meta) = file.metadata() {
            use std::os::unix::fs::PermissionsExt as _;
            LAST_TEMP_MODE.store(meta.permissions().mode() & 0o777, Ordering::Relaxed);
        }
        file.write_all(new)
            .with_context(|| format!("failed to write {}", temp.display()))?;
        drop(file);
        if let Some(mode) = target_mode {
            std::fs::set_permissions(&temp, mode)
                .with_context(|| format!("failed to keep {}'s mode", path.display()))?;
        }
        std::fs::rename(&temp, path)
            .with_context(|| format!("failed to move {} into place", path.display()))
    })();
    if result.is_err() {
        drop(std::fs::remove_file(&temp));
    }
    result
}

/// Remove the file when it still holds the bytes the pass read; a save that
/// landed since then is the user's, so the file stays.
fn remove_if_unchanged(path: &Path, old: Option<&[u8]>) -> Result<()> {
    unchanged_since_read(path, old)?;
    std::fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
}

/// Write the state a plan makes to `file`: clauth's own file atomically, a
/// block into the user's file in place, and a write that moves the user's own
/// bytes through a temp and a rename.
fn write_state(
    file: &Path,
    old: Option<&[u8]>,
    new: &[u8],
    held: &Held,
    user_tail: bool,
) -> Result<()> {
    match (held, old) {
        (Held::File { .. }, _) => write_atomic(file, old, new),
        (Held::Block { .. }, Some(old)) if user_tail => write_atomic(file, Some(old), new),
        (_, Some(old)) => edit_in_place(file, old, new),
        (_, None) => bail!(
            "{} is not there, so clauth has nowhere to put its block",
            file.display()
        ),
    }
}

/// Put the `prior` state back over the `current` bytes clauth wrote to
/// `file`: the prior file, the user's bytes, or no file when there was none.
/// A write that changed the file since clauth's own is the user's, so it
/// stays.
fn restore(file: &Path, prior: &Held, existing: Option<&[u8]>, current: &[u8]) -> Result<()> {
    match (prior, existing) {
        (Held::File { .. }, Some(prior)) => write_atomic(file, Some(current), prior),
        (_, Some(prior)) if current.starts_with(prior) => edit_in_place(file, current, prior),
        (_, Some(prior)) => write_atomic(file, Some(current), prior),
        (_, None) => remove_if_unchanged(file, Some(current)),
    }
}

/// One bounded herdr call, as a daemon-side spawn makes it: the pane's
/// session vars stripped, so `--session` alone picks the server.
fn run_herdr(bin: &Path, args: &[&str]) -> Option<Output> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::herdr::strip_session_env(&mut cmd);
    let child = cmd.spawn().ok()?;
    crate::herdr::run_bounded(child, crate::herdr::PROBE_TIMEOUT)
}

/// The loader check's scratch config root: a private dir under `~/.clauth`
/// holding the candidate as herdr's Claude override and the empty screen
/// herdr explains, removed when it drops.
struct LoaderScratch {
    root: tempfile::TempDir,
}

impl LoaderScratch {
    fn path(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> PathBuf {
        self.path().join("h")
    }

    fn config_home(&self) -> PathBuf {
        self.path().join("c")
    }

    fn state_home(&self) -> PathBuf {
        self.path().join("s")
    }

    fn override_path(&self, app: &str) -> PathBuf {
        self.config_home()
            .join(app)
            .join(OVERRIDE_REL[0])
            .join(OVERRIDE_REL[1])
    }

    fn screen(&self) -> PathBuf {
        self.path().join("screen")
    }
}

/// Build the loader check's scratch under `scratch_parent`, every dir 0700 and
/// every file 0600 from birth: it holds herdr's manifest and the user's
/// override bytes while the check runs, so nothing in it may be readable by
/// another local user even for an instant.
fn loader_scratch(scratch_parent: &Path, candidate: &[u8], app: &str) -> Result<LoaderScratch> {
    crate::profile::mkdir_700(scratch_parent)
        .with_context(|| format!("failed to create {}", scratch_parent.display()))?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".herdr-detect-");
    #[cfg(unix)]
    builder.permissions({
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::Permissions::from_mode(0o700)
    });
    let root = builder
        .tempdir_in(scratch_parent)
        .context("failed to create the loader check's scratch dir")?;
    let scratch = LoaderScratch { root };
    let file = scratch.override_path(app);
    crate::profile::mkdir_700(file.parent().unwrap_or_else(|| scratch.path()))
        .with_context(|| format!("failed to create {}", file.display()))?;
    crate::profile::atomic_write_600(&file, candidate)
        .with_context(|| format!("failed to write {}", file.display()))?;
    crate::profile::atomic_write_600(&scratch.screen(), b"")
        .with_context(|| format!("failed to write {}", scratch.screen().display()))?;
    Ok(scratch)
}

/// `herdr agent explain --json`'s answer: the manifest source it read.
#[derive(Deserialize)]
struct ExplainAnswer {
    manifest_source: String,
}

/// Whether herdr's own loader serves `candidate` as Claude's override:
/// `agent explain --file` runs the loader in the CLI process, here over a
/// scratch config root holding only the candidate, named as herdr's own app
/// dir (`app`) so a debug build finds it, with the env cleared so no server
/// and no real config is in play. An error means herdr gave no verdict.
fn herdr_loads(bin: &Path, candidate: &[u8], scratch_parent: &Path, app: &str) -> Result<bool> {
    let scratch = loader_scratch(scratch_parent, candidate, app)?;
    let file = scratch.override_path(app);
    let child = Command::new(bin)
        .args(["agent", "explain", "--file"])
        .arg(scratch.screen())
        .args(["--agent", "claude", "--json"])
        .env_clear()
        .env("HOME", scratch.home())
        .env("XDG_CONFIG_HOME", scratch.config_home())
        .env("XDG_STATE_HOME", scratch.state_home())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run herdr's loader check")?;
    let out = crate::herdr::run_bounded(child, crate::herdr::PROBE_TIMEOUT)
        .context("herdr's loader check timed out")?;
    if !out.status.success() {
        bail!("herdr's loader check exited {}", out.status);
    }
    let explain: ExplainAnswer = serde_json::from_slice(&out.stdout)
        .context("herdr's loader check printed no manifest answer")?;
    Ok(explain.manifest_source.as_str() == file.to_string_lossy().as_ref())
}

/// What reloading every running herdr server came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reload {
    /// No server runs: the next start loads the file.
    NoServer,
    /// A listing or a reload failed or did not answer.
    Failed,
    /// Every server reloaded; `by_override` is whether each now serves
    /// Claude from the local override.
    Loaded { by_override: bool },
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Session {
    name: String,
    running: bool,
}

#[derive(Deserialize)]
struct ReloadAnswer {
    result: ReloadResult,
}

#[derive(Deserialize)]
struct ReloadResult {
    manifests: Vec<ManifestInfo>,
}

#[derive(Deserialize)]
struct ManifestInfo {
    agent: String,
    source_kind: String,
}

/// `herdr server reload-agent-manifests` on every running server herdr's
/// `session list` names (one server per named session; all read one config
/// root), each reload's own answer saying which source serves Claude.
pub(crate) fn reload_all(bin: &Path) -> Reload {
    let Some(listed) = run_herdr(bin, &["session", "list", "--json"])
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<SessionList>(&out.stdout).ok())
    else {
        return Reload::Failed;
    };
    let running: Vec<String> = listed
        .sessions
        .into_iter()
        .filter(|session| session.running)
        .map(|session| session.name)
        .collect();
    if running.is_empty() {
        return Reload::NoServer;
    }
    let mut failed = false;
    let mut by_override = true;
    for name in &running {
        let answer = run_herdr(
            bin,
            &["--session", name, "server", "reload-agent-manifests"],
        )
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<ReloadAnswer>(&out.stdout).ok());
        match answer {
            Some(answer) => {
                by_override &= answer
                    .result
                    .manifests
                    .iter()
                    .any(|m| m.agent == "claude" && m.source_kind == "local override");
            }
            None => failed = true,
        }
    }
    if failed {
        Reload::Failed
    } else {
        Reload::Loaded { by_override }
    }
}

/// The cross-process lock every pass holds, with its rank in the global lock
/// order held for the same span, so a ranked acquisition under the pass trips
/// the debug assert instead of a deadlock. The file drops first, then the
/// rank.
struct HerdrLock {
    _file: File,
    _rank: crate::lockorder::RankGuard,
}

/// Take [`LOCK_FILE`] and enter its rank. Waiters give up after [`LOCK_WAIT`]
/// and the next tick or refresh retries.
fn hold_lock(clauth_dir: &Path) -> Result<HerdrLock> {
    crate::profile::mkdir_700(clauth_dir)
        .with_context(|| format!("failed to create {}", clauth_dir.display()))?;
    let file = crate::profile::open_state_file(&clauth_dir.join(LOCK_FILE))
        .context("failed to open the herdr detection lock")?;
    crate::lock::lock_file_with_timeout(
        &file,
        LOCK_WAIT,
        |waited| {
            anyhow::anyhow!(
                "timed out after {} ms waiting for another clauth process's herdr detection pass",
                waited.as_millis()
            )
        },
        "failed to take the herdr detection lock",
    )?;
    Ok(HerdrLock {
        _file: file,
        _rank: crate::lockorder::RankGuard::enter::<crate::lockorder::rank::HerdrDetect>(),
    })
}

/// One pass: bring the override in line with the toggle, under the lock. The
/// persisted toggle decides, read after the lock, so a pass queued behind a
/// flip's own pass follows the flip rather than the state its due was
/// computed from.
pub(crate) fn pass(ctx: &Ctx) -> Result<()> {
    let _lock = hold_lock(&ctx.clauth_dir)?;
    let on = crate::profile::load_app_state()?.herdr.held_question_dot;
    let record_path = ctx.record_path();
    let record = load_record(&record_path);
    if on {
        pass_on(ctx, &record_path, record.as_ref())
    } else {
        pass_off(ctx, &record_path, record.as_ref())
    }
}

/// The override to act on, with herdr's app dir name: the live config root
/// herdr answers with, else the path the record names when herdr cannot
/// answer. The live root wins over the record's, which a config-root move
/// leaves stale. `None` when neither exists.
fn resolve_override(ctx: &Ctx, record: Option<&Record>) -> Option<(PathBuf, String)> {
    let root = crate::herdr::config_root(&ctx.bin.to_string_lossy());
    if let Ok(root) = &root
        && let Some(app) = app_dir(root)
    {
        return Some((root.join(OVERRIDE_REL[0]).join(OVERRIDE_REL[1]), app));
    }
    let fallback = record
        .and_then(|record| record.override_path.as_deref())
        .and_then(|path| app_dir_of_override(path).map(|app| (path.to_path_buf(), app)));
    if fallback.is_none()
        && let Err(e) = root
    {
        log_pass_error(&e);
    }
    fallback
}

fn pass_on(ctx: &Ctx, record_path: &Path, record: Option<&Record>) -> Result<()> {
    let Some((path, app)) = resolve_override(ctx, record) else {
        // Nothing names a root to act on; the cache the next due check reads
        // is recorded, so this state does not start a pass per cadence.
        let seen = remote_manifest_path(APP_DIR)
            .as_deref()
            .and_then(read_cache)
            .map(|cache| cache.version);
        return save_record(
            record_path,
            &Record {
                override_path: None,
                held: Held::Unapplied,
                seen,
                failure: None,
                attempted: None,
                refused: None,
            },
        );
    };
    let cache = remote_manifest_path(&app).as_deref().and_then(read_cache);
    let seen = cache.as_ref().map(|cache| cache.version.clone());
    let prior = record.map_or(Held::Unapplied, |record| record.held.clone());
    let existing = match read_override(&path) {
        Ok(existing) => existing,
        Err(e) => {
            // A file clauth cannot read or resolve stays as it is; the refusal
            // is remembered, so no later check starts a pass for it until the
            // file, the cached version or the toggle moves.
            save_refusal(
                record_path,
                Some(&path),
                &prior,
                seen,
                unread_refusal(&path),
            )?;
            return Err(e);
        }
    };
    let plan = match plan_on(
        existing.bytes.as_deref(),
        existing.linked,
        Some(&prior),
        cache.as_ref(),
    ) {
        Ok(plan) => plan,
        Err(e) => {
            // A file clauth cannot apply over stays as it is; the refusal is
            // remembered until the file, the cached version or the toggle
            // moves.
            save_refusal(
                record_path,
                Some(&path),
                &prior,
                seen,
                bytes_refusal(existing.bytes.as_deref()),
            )?;
            return Err(e);
        }
    };
    let (held, failure, attempted) = match plan {
        OnPlan::Keep(held) => (held, None, None),
        OnPlan::Write {
            bytes,
            held,
            user_tail,
        } => {
            let block = match &held {
                Held::File { block, .. } | Held::Block { block } => Some(block.clone()),
                Held::HandKept | Held::Unapplied => None,
            };
            let (held, failure) = match herdr_loads(&ctx.bin, &bytes, &ctx.clauth_dir, &app) {
                Ok(true) => match apply(
                    ctx,
                    &existing.file,
                    existing.bytes.as_deref(),
                    &bytes,
                    held,
                    &prior,
                    user_tail,
                ) {
                    Ok(done) => done,
                    Err(e) => {
                        // A write or restore the pass could not make is
                        // remembered as a refusal: the next check waits for
                        // the file, the cache or the toggle to move.
                        save_refusal(
                            record_path,
                            Some(&path),
                            &prior,
                            seen,
                            bytes_refusal(existing.bytes.as_deref()),
                        )?;
                        return Err(e);
                    }
                },
                Ok(false) => (prior, Some(Failure::LoadFailed)),
                Err(e) => {
                    log_pass_error(&e);
                    (prior, Some(Failure::LoadFailed))
                }
            };
            let attempted = if failure == Some(Failure::LoadFailed) {
                block
            } else {
                None
            };
            (held, failure, attempted)
        }
    };
    save_record(
        record_path,
        &Record {
            override_path: Some(path),
            held,
            seen,
            failure,
            attempted,
            refused: None,
        },
    )
}

/// Write `bytes` to `file`, reload, and put `existing` back when a server did
/// not load the result.
fn apply(
    ctx: &Ctx,
    file: &Path,
    existing: Option<&[u8]>,
    bytes: &[u8],
    held: Held,
    prior: &Held,
    user_tail: bool,
) -> Result<(Held, Option<Failure>)> {
    write_state(file, existing, bytes, &held, user_tail)?;
    Ok(match reload_all(&ctx.bin) {
        Reload::NoServer | Reload::Loaded { by_override: true } => (held, None),
        Reload::Failed => (held, Some(Failure::NoReload)),
        Reload::Loaded { by_override: false } => {
            restore(file, prior, existing, bytes)?;
            let _ = reload_all(&ctx.bin);
            (prior.clone(), Some(Failure::LoadFailed))
        }
    })
}

fn pass_off(ctx: &Ctx, record_path: &Path, record: Option<&Record>) -> Result<()> {
    // With no record, the override path is resolved from herdr's live root
    // (or nothing is known to undo) and no file is known to be clauth's: a
    // user override loses exactly clauth's intact block, and any other file
    // is left alone.
    if let Some((path, _app)) = resolve_override(ctx, record) {
        let prior = record.map_or(Held::Unapplied, |record| record.held.clone());
        let seen = record.and_then(|record| record.seen.clone());
        let existing = match read_override(&path) {
            Ok(existing) => existing,
            Err(e) => {
                // A file clauth cannot read or resolve stays as it is; the
                // refusal keeps the next check from retrying until the file
                // moves.
                save_refusal(
                    record_path,
                    Some(&path),
                    &prior,
                    seen,
                    unread_refusal(&path),
                )?;
                return Err(e);
            }
        };
        let plan = match plan_off(
            existing.bytes.as_deref(),
            existing.linked,
            record.map(|record| &record.held),
        ) {
            Ok(plan) => plan,
            Err(e) => {
                // A file clauth cannot undo exactly stays as it is; the record
                // still goes, so the refusal is not retried every cadence.
                log_pass_error(&e);
                OffPlan::Keep
            }
        };
        let changed = match plan {
            OffPlan::Delete => match remove_if_unchanged(&existing.file, existing.bytes.as_deref())
            {
                Ok(()) => true,
                Err(e) => {
                    save_refusal(
                        record_path,
                        Some(&path),
                        &prior,
                        seen,
                        bytes_refusal(existing.bytes.as_deref()),
                    )?;
                    return Err(e);
                }
            },
            OffPlan::Write { bytes, user_tail } => {
                let result = match existing.bytes.as_deref() {
                    Some(old) if user_tail => write_atomic(&existing.file, Some(old), &bytes),
                    Some(old) => edit_in_place(&existing.file, old, &bytes),
                    None => Err(anyhow::anyhow!(
                        "{} is gone, so clauth has no block to remove",
                        path.display()
                    )),
                };
                match result {
                    Ok(()) => true,
                    Err(e) => {
                        save_refusal(
                            record_path,
                            Some(&path),
                            &prior,
                            seen,
                            bytes_refusal(existing.bytes.as_deref()),
                        )?;
                        return Err(e);
                    }
                }
            }
            OffPlan::Keep => false,
        };
        if changed {
            let _ = reload_all(&ctx.bin);
        }
    }
    match std::fs::remove_file(record_path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("failed to remove {}", record_path.display()))
        }
        _ => Ok(()),
    }
}

/// The text of the last pass error logged: a failure that repeats on every
/// cadence (a herdr that cannot answer, an unreadable override) writes one
/// line, the way the daemon's config reload dedups its own.
static LAST_PASS_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn log_pass_error(error: &anyhow::Error) {
    let text = format!("{error:#}");
    let mut last = LAST_PASS_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() != Some(text.as_str()) {
        logline!("clauth: herdr held question dot: {text}");
        *last = Some(text);
    }
}

fn clear_pass_error() {
    *LAST_PASS_ERROR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Whether the override no longer carries the state the record says it holds:
/// a kept file that lost the hand-kept rule or clauth's block, a regular file
/// clauth wrote and someone edited, or a file the watcher cannot read — a
/// deleted one included — one pass is then due, whatever the caches hold, and
/// its refusal gates the retry. The watcher reads the file back (never a
/// subprocess), so the next pass plans from the file as it now is. Nothing
/// applied holds no state to lose, and a refusal is
/// [`refused_file_changed`]'s: clauth touched no such file.
fn held_state_moved(record: Option<&Record>) -> bool {
    record.is_some_and(|record| {
        if record.refused.is_some() {
            return false;
        }
        let Some(path) = record.override_path.as_deref() else {
            return false;
        };
        let bytes = read_regular(path).ok();
        match &record.held {
            Held::HandKept => bytes.is_none_or(|bytes| !has_rule_id(&bytes)),
            Held::Block { block } => {
                bytes.is_none_or(|bytes| find(&bytes, block.as_bytes()).is_none())
            }
            Held::File { digest, .. } => bytes.is_none_or(|bytes| sha_hex(&bytes) != *digest),
            Held::Unapplied => false,
        }
    })
}

/// What a refusal records when the override could not be read: the file stays
/// as it is until it can be read.
const REFUSED_UNREADABLE: &str = "unreadable";
/// The same when there was no file to read.
const REFUSED_ABSENT: &str = "absent";

/// What a refusal records after reading the override: the digest of the bytes
/// it read, or the marker for no file.
fn bytes_refusal(existing: Option<&[u8]>) -> String {
    existing.map_or_else(|| REFUSED_ABSENT.to_string(), sha_hex)
}

/// What a refusal records when the override could not be read: the target a
/// link there names, so retargeting it is the change that retries the pass,
/// else the marker for an unreadable file.
fn unread_refusal(path: &Path) -> String {
    link_marker(path).unwrap_or_else(|| REFUSED_UNREADABLE.to_string())
}

/// The link at `path`, spelled by the target it names.
fn link_marker(path: &Path) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|target| format!("link:{}", target.display()))
}

/// Whether the override a refusal was recorded against moved since: the user
/// edited it, removed it, retargeted the link or made it readable again. The
/// watcher reads the file back (never a subprocess), so the refusal is
/// retried once the user's edit can make the plan work; a file that stays
/// unreadable and a link that stays dangling start no pass.
fn refused_file_changed(record: Option<&Record>) -> bool {
    record.is_some_and(|record| {
        let Some(refused) = record.refused.as_deref() else {
            return false;
        };
        let Some(path) = record.override_path.as_deref() else {
            return false;
        };
        match read_regular(path) {
            Ok(bytes) => sha_hex(&bytes) != refused,
            // Nothing readable there: an absent file is a change unless the
            // refusal recorded the absence, and a link is a change once it
            // names another target.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                link_marker(path).map_or(refused != REFUSED_ABSENT, |marker| marker != refused)
            }
            Err(_) => link_marker(path).is_some_and(|marker| marker != refused),
        }
    })
}

/// The pass this process should run now, or `None` when none is due: the
/// tick's and the Services tab's watcher. With the toggle off it reads clauth's
/// own record and the override the record names; while on it also reads the
/// cache under the record's app dir and the override itself, never a
/// subprocess.
pub(crate) fn due_pass(on: bool) -> Option<Ctx> {
    let clauth_dir = crate::profile::clauth_dir().ok()?;
    let record = load_record(&clauth_dir.join(RECORD_FILE));
    let app = record
        .as_ref()
        .and_then(|record| record.override_path.as_deref())
        .and_then(app_dir_of_override)
        .unwrap_or_else(|| APP_DIR.to_string());
    let is_due = due(on, record.as_ref(), || {
        remote_manifest_path(&app)
            .as_deref()
            .and_then(read_cache)
            .map(|cache| cache.version)
    }) || refused_file_changed(record.as_ref())
        || held_state_moved(record.as_ref());
    if !is_due {
        return None;
    }
    Some(Ctx {
        bin: crate::herdr::resolved_bin()?,
        clauth_dir,
    })
}

/// One pass whatever is due, for the toggle's flip: the persisted toggle
/// decides, so the pass follows the state the flip stored. Returns the card
/// the pass leaves.
pub(crate) fn run_now() -> Vec<String> {
    let on = crate::profile::load_app_state().is_ok_and(|state| state.herdr.held_question_dot);
    if let Some(bin) = crate::herdr::resolved_bin()
        && let Ok(clauth_dir) = crate::profile::clauth_dir()
    {
        let ctx = Ctx { bin, clauth_dir };
        match pass(&ctx) {
            Ok(()) => clear_pass_error(),
            Err(e) => log_pass_error(&e),
        }
    }
    card(on)
}

/// A pass a detached thread runs, so two never overlap in one process.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Clears [`IN_FLIGHT`] when a guarded pass ends, a panic included.
struct InFlightCleared;

impl Drop for InFlightCleared {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Run `run` unless a pass is already in flight in this process; a second
/// pass would only wait out the lock, and the next tick or refresh retries.
fn run_guarded(run: impl FnOnce()) {
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let _cleared = InFlightCleared;
    run();
}

/// The daemon tick's leg: a due pass runs off the tick, so a slow herdr never
/// spends the tick's watchdog window. A test build runs it inline.
pub(crate) fn watch_detached(on: bool) {
    let Some(ctx) = due_pass(on) else {
        return;
    };
    let run = move || match pass(&ctx) {
        Ok(()) => clear_pass_error(),
        Err(e) => log_pass_error(&e),
    };
    #[cfg(test)]
    run_guarded(run);
    #[cfg(not(test))]
    if let Err(e) = std::thread::Builder::new()
        .name("clauth-herdr-detect".into())
        .spawn(move || run_guarded(run))
    {
        log_pass_error(&anyhow::Error::new(e));
    }
}

#[cfg(test)]
#[path = "../tests/inline/herdr_detect.rs"]
pub(crate) mod tests;
