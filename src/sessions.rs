//! Session index over Claude Code transcript stores.
//!
//! Builds a newest-first, workspace-grouped inventory of CC sessions across the
//! global `~/.claude/projects/` store plus every live isolated runtime's own
//! store. The cost ceiling is deliberate: a session's first and last user
//! message come from a bounded HEAD read and a seek-from-end TAIL read of each
//! JSONL, never a full-transcript parse — the token subsystem already shows a
//! full parse is too heavy to run per index build.
//!
//! This is the A1 foundation: the index core plus preview redaction. Later
//! passes fill the remaining [`SessionInfo`] fields — A2 the per-session
//! `tokens`/`cost` annotation, A3 the `last_ran_profile` stamp — so those fields
//! are defined now but left `None` here.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::logline::logline;
use crate::pricing::PriceTable;
use crate::profile::{atomic_write_600, claude_dir, clauth_dir, mkdir_700};

/// Bytes read from a file's head to recover its workspace and first user
/// message. The session id comes from the filename stem, not the head, so this
/// window only has to reach the first user turn, which sits at or near the top.
const HEAD_MAX_BYTES: u64 = 256 * 1024;
/// Initial tail-read window scanned backward for the last user message.
const TAIL_CHUNK: u64 = 64 * 1024;
/// Ceiling the tail window grows to when a chunk holds no user line, bounding
/// the read on a transcript whose tail is all tool traffic.
const TAIL_MAX: u64 = 1024 * 1024;
/// Preview length cap, in characters (not bytes — truncation lands on a char
/// boundary so non-ASCII never panics).
const PREVIEW_MAX_CHARS: usize = 200;
/// Recursion cap for the `*.jsonl` walk. Subagent/workflow transcripts nest a
/// few levels under `projects/<slug>/<session>/`, so a shallow walk would miss
/// them; the cap bounds the descent and (with symlink dirs treated as files)
/// avoids cycles.
const WALK_MAX_DEPTH: usize = 8;
/// Walk depth that stops at `projects/<slug>/<id>.jsonl`, the only shape Claude
/// Code's `--resume` resolves. Everything the deeper walk adds is a nested
/// per-session tree (`subagents/`, `workflows/`, `tool-results/`) whose ids CC
/// answers no-match for, so a target that has to end in a spawned session picks
/// from this set and the browsing surfaces keep the full one.
const TOP_LEVEL_DEPTH: usize = 2;

/// The fixed mask a secret-shaped substring is replaced with.
const MASK: &str = "[REDACTED]";

/// Which store a session's transcript lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionSource {
    /// The shared global store `~/.claude/projects/`.
    Global,
    /// A live isolated runtime's own throwaway store, tagged with its profile.
    Isolated { profile: String },
}

/// One indexed session.
///
/// `tokens`, `cost`, and `last_ran_profile` are populated by later passes (A2
/// token/cost annotation, A3 profile tracking) and are left `None` here. They
/// stay `Option` on purpose: a session missing from the token stats or the
/// last-ran map renders blank, never `0`/empty-string.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    /// The session id — the transcript filename stem (`<sessionId>.jsonl`), the
    /// id `claude --resume <id>` resolves by. NOT the in-file `sessionId`, which
    /// a resume copy carries forward from its parent. Deliberately not redacted
    /// (a UUID); only the message previews below are.
    pub(crate) id: String,
    /// The workspace, taken from the transcript line's own `cwd` value — the
    /// authoritative source. The dashed dir-slug under `projects/` is lossy and
    /// deliberately not decoded back into a path. Deliberately not redacted: it
    /// is the grouping key and a user-chosen filesystem path; masking it would
    /// break grouping and gut the path display. Only the previews below are.
    pub(crate) workspace: String,
    /// Source file path — the tie-breaker when the same session id shows up in
    /// two stores at an equal mtime. Module-private: consumers key off `id`, and
    /// a caller that wants one session's path takes [`find_session`].
    path: PathBuf,
    /// File mtime — a cheap freshness key that needs no parse.
    pub(crate) updated: SystemTime,
    /// First user message, redacted preview (`None` when the head held none).
    pub(crate) first_message: Option<String>,
    /// Last user message, redacted preview (`None` when the tail held none).
    pub(crate) last_message: Option<String>,
    /// Which store the transcript came from.
    #[allow(dead_code, reason = "written at index time; read by the Sessions tab")]
    pub(crate) source: SessionSource,
    /// Per-session token total — A2 fills this; `None` = absent from stats.
    pub(crate) tokens: Option<u64>,
    /// API-equivalent cost in USD — A2 fills this; `None` = unpriced/absent.
    pub(crate) cost: Option<f64>,
    /// Profile the session last ran under — A3 fills this; `None` = unknown.
    pub(crate) last_ran_profile: Option<String>,
}

/// Sessions that share one workspace (`cwd`), newest-first within the group.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceGroup {
    pub(crate) workspace: String,
    pub(crate) sessions: Vec<SessionInfo>,
}

/// Compile a static redaction pattern. Each pattern is a compile-time constant,
/// so an `Err` is a code bug the module's own tests catch, never a runtime path.
#[allow(
    clippy::expect_used,
    reason = "static redaction pattern is a valid regex"
)]
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid redaction regex")
}

// Layered preview redaction. Ordering is load-bearing: the precise provider
// rules (A) and the key/value rules (B) run first and drop `[REDACTED]` in
// place; the generic entropy catch-all (C) runs last over whatever survives.
// The mask holds `[` `]`, both outside Layer C's class, so C never re-touches an
// A/B mask. Err toward over-redaction: a false positive is cosmetic, a leaked
// key is not.

// --- Layer A: explicit high-confidence provider/token shapes ---
// Anthropic / OpenAI secret key.
static SK_KEY: LazyLock<Regex> = LazyLock::new(|| compile(r"\bsk-[A-Za-z0-9_-]{8,}"));
// GitHub token (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`) and fine-grained PAT. A
// leading `_` is a word char, so a `\b`-anchored generic blob misses these.
static GITHUB_TOKEN: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}"));
static GITHUB_PAT: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}"));
// Slack token — dash-split, and `-` is not a word char.
static SLACK_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\bxox[baprs]-[A-Za-z0-9-]{10,}"));
// Google API key.
static GOOGLE_API: LazyLock<Regex> = LazyLock::new(|| compile(r"\bAIza[A-Za-z0-9_-]{10,}"));
// AWS access key id.
static AWS_AKID: LazyLock<Regex> = LazyLock::new(|| compile(r"\bAKIA[0-9A-Z]{16}\b"));
// JWT — masked as one unit so a dot-split never leaves two live halves.
static JWT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"));
// `Bearer <token>` — keep the marker (group 1), mask the token.
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{8,}"));
// URL credentials `scheme://user:pass@host` — keep user + host, mask the password.
static URL_CREDS: LazyLock<Regex> = LazyLock::new(|| compile(r"(://[^\s:/@]+:)([^\s/@]+)(@)"));

// --- Layer B: key/value pairs — keep the key, mask the value (group 2) ---
static KV_JSON: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)("[a-z0-9_.-]*(?:token|secret|password|api[_-]?key|authorization)[a-z0-9_.-]*"\s*:\s*")([^"]*)(")"#,
    )
});
static KV_ENV: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)\b([a-z0-9_.-]*(?:token|secret|password|api[_-]?key|authorization)[a-z0-9_.-]*\s*=\s*)([^\s"']+)"#,
    )
});

// --- Layer C: generic high-entropy catch-all, filtered in a closure ---
// `regex` has no lookahead, so the entropy test runs per match inside the
// `replace_all` closure, not as a pattern. The class omits `.` and whitespace so
// a run can't span sentence or path-dot boundaries.
static ENTROPY_BLOB: LazyLock<Regex> = LazyLock::new(|| compile(r"[A-Za-z0-9+/=_-]{24,}"));

/// A generic run "looks secret" when it clears the length floor and mixes at
/// least one digit with one letter — sparing pure-word path segments
/// (`gettingstartedguide`) and pure-number runs, while still catching random
/// tokens, url-safe base64, and git SHAs.
fn looks_secret(run: &str) -> bool {
    run.len() >= 24
        && run.bytes().any(|b| b.is_ascii_digit())
        && run.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Mask secret-shaped substrings in preview text. Applied to the in-memory
/// preview only; the source JSONL is never touched, so redaction is one-way at
/// the render boundary.
pub(crate) fn redact_secrets(s: &str) -> String {
    // Layer A: precise provider/token shapes (whole match, or keep a marker).
    let mut out = SK_KEY.replace_all(s, MASK).into_owned();
    out = GITHUB_TOKEN.replace_all(&out, MASK).into_owned();
    out = GITHUB_PAT.replace_all(&out, MASK).into_owned();
    out = SLACK_TOKEN.replace_all(&out, MASK).into_owned();
    out = GOOGLE_API.replace_all(&out, MASK).into_owned();
    out = AWS_AKID.replace_all(&out, MASK).into_owned();
    out = JWT.replace_all(&out, MASK).into_owned();
    out = BEARER.replace_all(&out, "${1}[REDACTED]").into_owned();
    out = URL_CREDS
        .replace_all(&out, "${1}[REDACTED]${3}")
        .into_owned();

    // Layer B: recognizable key/value pairs — key stays, value masked.
    out = KV_JSON.replace_all(&out, "${1}[REDACTED]${3}").into_owned();
    out = KV_ENV.replace_all(&out, "${1}[REDACTED]").into_owned();

    // Layer C: entropy catch-all over what survived, filtered so file paths and
    // pure-word identifiers pass through unmasked.
    out = ENTROPY_BLOB
        .replace_all(&out, |caps: &regex::Captures| {
            let run = &caps[0];
            if looks_secret(run) {
                MASK.to_string()
            } else {
                run.to_string()
            }
        })
        .into_owned();
    out
}

/// A transcript line, decoded just far enough for the index. Unknown fields are
/// ignored, so tool-use / summary / meta lines parse without error and simply
/// yield no user text.
#[derive(Deserialize)]
struct TranscriptLine {
    cwd: Option<String>,
    #[allow(dead_code)]
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    role: Option<String>,
    content: Option<Content>,
}

/// User `content` is either a plain string or an array of typed blocks; a
/// catch-all keeps an unexpected shape from failing the whole line.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
    // Only consumes an unexpected shape so the whole line still parses; its
    // value is intentionally never read.
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

/// The redacted preview of a line's user text, or `None` when the line is not a
/// user turn or carries no text (e.g. a `tool_result`-only line).
fn user_text(line: &TranscriptLine) -> Option<String> {
    let msg = line.message.as_ref()?;
    if msg.role.as_deref() != Some("user") {
        return None;
    }
    let raw = match msg.content.as_ref()? {
        Content::Text(s) => s.clone(),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter(|b| b.kind.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join(""),
        Content::Other(_) => return None,
    };
    preview_of(&raw)
}

/// Redact then truncate to a bounded, char-boundary-safe preview. Redaction runs
/// on the full text first so a secret can never survive by straddling the
/// truncation point.
fn preview_of(raw: &str) -> Option<String> {
    let redacted = redact_secrets(raw.trim());
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(crate::format::truncate(trimmed, PREVIEW_MAX_CHARS))
}

/// Head metadata recovered from a transcript's first lines. The session id is
/// keyed off the filename, not the head, so it is absent here.
#[derive(Default)]
struct Head {
    workspace: String,
    first_message: Option<String>,
}

/// Read a bounded head window for the workspace (`cwd`) and first user message.
/// Best-effort: an unreadable file, or a head carrying neither, yields an empty
/// workspace / `None` message rather than dropping the session — its id comes
/// from the filename, so a summary-first or oversized head is still indexed.
fn read_head(path: &Path) -> Head {
    let Ok(file) = File::open(path) else {
        return Head::default();
    };
    let reader = BufReader::new(file.take(HEAD_MAX_BYTES));
    let mut cwd: Option<String> = None;
    let mut first_message: Option<String> = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(c) = parsed.cwd.as_deref().filter(|c| !c.is_empty())
        {
            cwd = Some(c.to_string());
        }
        if first_message.is_none() {
            first_message = user_text(&parsed);
        }
        if cwd.is_some() && first_message.is_some() {
            break;
        }
    }
    Head {
        workspace: cwd.unwrap_or_default(),
        first_message,
    }
}

/// The last user message, found by seeking from the end and scanning a bounded
/// tail window backward — never a full parse. The window grows up to [`TAIL_MAX`]
/// if a chunk holds only tool traffic. Fail-soft: any IO error yields `None`.
fn read_last_user_message(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let mut window = TAIL_CHUNK;
    loop {
        let read_len = window.min(len);
        file.seek(SeekFrom::Start(len - read_len)).ok()?;
        let mut buf = Vec::with_capacity(read_len as usize);
        file.by_ref().take(read_len).read_to_end(&mut buf).ok()?;

        // Unless the window starts at byte 0, its first line is a partial cut —
        // drop up to the first newline so every scanned line is whole.
        let slice: &[u8] = if read_len < len {
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => &buf[i + 1..],
                None => &buf[..],
            }
        } else {
            &buf[..]
        };

        if let Some(msg) = last_user_in_slice(slice) {
            return Some(msg);
        }
        // Whole file already covered, or the window hit its ceiling: give up.
        if read_len >= len || window >= TAIL_MAX {
            return None;
        }
        window = (window * 2).min(TAIL_MAX);
    }
}

/// Scan `slice`'s lines back-to-front, returning the first (i.e. latest) user
/// message text.
fn last_user_in_slice(slice: &[u8]) -> Option<String> {
    for line in slice.split(|&b| b == b'\n').rev() {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(text) else {
            continue;
        };
        if let Some(msg) = user_text(&parsed) {
            return Some(msg);
        }
    }
    None
}

/// The session id: the transcript filename stem. CC names each transcript
/// `<sessionId>.jsonl` and `--resume <id>` resolves by that stem, so it is the
/// authoritative id — unlike the in-file `sessionId`, which a resume copy
/// carries forward from its parent. Should CC ever emit a `<id>.summary.jsonl`,
/// the stem `<id>.summary` is taken verbatim; plain `<uuid>.jsonl` is the norm.
fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

/// A file's mtime — the freshness key every ordering here is built on. `None`
/// when it can't be read, which drops the file from that ordering rather than
/// ranking it at an invented time.
fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Index one file into a [`SessionInfo`], or `None` when it has no usable
/// filename stem or its metadata can't be read. Head metadata is best-effort.
fn scan_file(path: &Path, source: &SessionSource) -> Option<SessionInfo> {
    let id = session_id_from_path(path)?;
    let updated = mtime_of(path)?;
    let head = read_head(path);
    Some(SessionInfo {
        id,
        workspace: head.workspace,
        path: path.to_path_buf(),
        updated,
        first_message: head.first_message,
        last_message: read_last_user_message(path),
        source: source.clone(),
        tokens: None,
        cost: None,
        last_ran_profile: None,
    })
}

/// Recursively collect `*.jsonl` paths under `dir` (depth-capped). A symlinked
/// directory is treated as a file and never descended, bounding the walk.
///
/// Returns `false` when any directory could not be read — the depth cap
/// truncating a subtree included — so a caller whose answer depends on seeing
/// EVERY transcript (the prune) can refuse rather than mistake a partial walk
/// for a complete one. A skipped symlinked directory hides a subtree the same
/// way, so it too marks the walk incomplete. The collected paths are still
/// returned: the read paths keep their fail-soft behavior and ignore the flag.
fn collect_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> bool {
    if depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut complete = true;
    for entry in entries {
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            complete = false;
            continue;
        };
        let path = entry.path();
        if file_type.is_symlink() {
            // Never followed (bounds the walk), but a link hides whatever it
            // points at, so the walk did not see everything under it.
            complete = false;
        }
        if file_type.is_dir() {
            if !collect_jsonl(&path, depth - 1, out) {
                complete = false;
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    complete
}

/// Index every `*.jsonl` under one store's `projects/` dir into `by_id`, keeping
/// the newest entry when a session id appears in more than one file.
fn index_store(projects: &Path, source: &SessionSource, by_id: &mut HashMap<String, SessionInfo>) {
    let mut paths = Vec::new();
    collect_jsonl(projects, WALK_MAX_DEPTH, &mut paths);
    for path in paths {
        if let Some(info) = scan_file(&path, source) {
            insert_newest(by_id, info);
        }
    }
}

/// Collapse a duplicate session id (the same `<id>.jsonl` copied into more than
/// one store or project-slug dir) to the newest by mtime. On an equal mtime the
/// lexicographically greater source path wins, so the pick stays stable
/// regardless of `read_dir` order.
fn insert_newest(map: &mut HashMap<String, SessionInfo>, info: SessionInfo) {
    match map.entry(info.id.clone()) {
        Entry::Occupied(mut e) => {
            let cur = e.get();
            let wins =
                info.updated > cur.updated || (info.updated == cur.updated && info.path > cur.path);
            if wins {
                e.insert(info);
            }
        }
        Entry::Vacant(e) => {
            e.insert(info);
        }
    }
}

/// Group sessions by workspace, newest-first within each group and groups
/// ordered by their newest session. The session id is the stable tie-breaker so
/// equal mtimes still order deterministically.
fn group_by_workspace(sessions: Vec<SessionInfo>) -> Vec<WorkspaceGroup> {
    let mut groups: HashMap<String, Vec<SessionInfo>> = HashMap::new();
    for s in sessions {
        groups.entry(s.workspace.clone()).or_default().push(s);
    }
    let mut out: Vec<WorkspaceGroup> = groups
        .into_iter()
        .map(|(workspace, mut sessions)| {
            sessions.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
            WorkspaceGroup {
                workspace,
                sessions,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let an = a.sessions.first().map(|s| s.updated);
        let bn = b.sessions.first().map(|s| s.updated);
        bn.cmp(&an).then_with(|| a.workspace.cmp(&b.workspace))
    });
    out
}

/// Build the session index: the global store plus every live isolated runtime's
/// own store, deduped by session id and grouped by workspace, newest-first.
/// Fail-soft throughout — an unreadable file or store is skipped, never fatal.
pub(crate) fn build_index() -> Vec<WorkspaceGroup> {
    let mut by_id: HashMap<String, SessionInfo> = HashMap::new();

    if let Ok(projects) = claude_dir().map(|d| d.join("projects")) {
        index_store(&projects, &SessionSource::Global, &mut by_id);
    }
    for (profile, projects) in crate::runtime::live_isolated_stores() {
        index_store(&projects, &SessionSource::Isolated { profile }, &mut by_id);
    }

    group_by_workspace(by_id.into_values().collect())
}

// ── Targeted lookup: one session, without the index's per-transcript reads ────
//
// [`build_index`] head- AND tail-reads every transcript to build previews, which
// a by-id caller then throws away: 11.3 s for `clauth info latest` over a
// 12k-session store, against 44 ms for the same answer here. The lookups below
// walk that store for filenames and mtimes, then read the head of the ONE file
// they resolved to.
//
// The GLOBAL store only, unlike the index. A live isolated runtime's own store
// belongs to a session another process is running; `clauth resume` and the
// `delegate` resume both spawn against the shared store, where Claude Code would
// answer `No conversation found` for an id that only exists in an isolated tree.
// An isolated run's transcript reaches this store once the rescue lifts it out.

/// One session located by id, carrying only what a targeted caller needs. The
/// [`SessionInfo`] fields absent here — previews, token totals — are exactly the
/// per-transcript reads this lookup exists to avoid.
#[derive(Debug, Clone)]
pub(crate) struct SessionRef {
    /// The transcript filename stem: the id `--resume` resolves by.
    pub(crate) id: String,
    /// The transcript's on-disk path.
    pub(crate) path: PathBuf,
    /// File mtime — free here (the walk stats every candidate to order them)
    /// and what a caller compares one store's answer against another's.
    pub(crate) updated: SystemTime,
}

/// A transcript in a live isolated runtime's own store: real, listed by
/// `clauth sessions`, and unreachable by a resume until that run ends and the
/// rescue lifts it into the shared store.
#[derive(Debug, Clone)]
pub(crate) struct IsolatedHold {
    pub(crate) session: SessionRef,
    /// The profile whose live isolated run owns that store.
    pub(crate) profile: String,
}

impl SessionRef {
    /// The workspace this session was recorded in, from the head of its own
    /// transcript. `None` when the head records no `cwd` — a resume then has
    /// nowhere to run, the same dead end as no transcript at all.
    pub(crate) fn workspace(&self) -> Option<PathBuf> {
        let workspace = read_head(&self.path).workspace;
        (!workspace.is_empty()).then(|| PathBuf::from(workspace))
    }
}

/// Every `*.jsonl` path in the global store, none of them opened. `depth` picks
/// how much of it the caller means: [`WALK_MAX_DEPTH`] for every transcript the
/// listing shows, [`TOP_LEVEL_DEPTH`] for the ones a resume can reach.
fn global_transcripts(depth: usize) -> Vec<PathBuf> {
    let Ok(projects) = claude_dir().map(|d| d.join("projects")) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    collect_jsonl(&projects, depth, &mut paths);
    paths
}

/// Locate one session by exact id. When the same id sits in more than one
/// project-slug dir, the newest mtime wins and an equal mtime falls back to the
/// greater path — [`insert_newest`]'s rule, so a targeted lookup and the index
/// resolve one id to the same file.
pub(crate) fn find_session(session_id: &str) -> Option<SessionRef> {
    let (updated, path) = global_transcripts(WALK_MAX_DEPTH)
        .into_iter()
        .filter(|p| session_id_from_path(p).as_deref() == Some(session_id))
        .filter_map(|p| mtime_of(&p).map(|t| (t, p)))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))?;
    Some(SessionRef {
        id: session_id.to_owned(),
        path,
        updated,
    })
}

/// The newest session in the store — what a `latest` target resolves to. The
/// ordering is the index's own newest-first key ([`group_by_workspace`] then
/// `flatten_newest_first`): greatest mtime, then the smallest id, then
/// [`insert_newest`]'s greater-path duplicate rule. Paths are unique, so the
/// comparison is total and the pick never depends on `read_dir` order.
///
/// Only [`TOP_LEVEL_DEPTH`] of the store, unlike the listing. `latest` has to
/// end in a session Claude Code will actually open, and CC resolves `--resume`
/// against a UUID or a session title: handed a nested transcript's `agent-<hex>`
/// stem it answers no-match, in `--print` as an error and interactively by
/// dropping the operator into the session picker with nothing selected
/// (observed on CC 2.1.221). So a store whose
/// newest file is a subagent transcript resolves `latest` to the newest session
/// under it, and the listing keeps naming that transcript first.
pub(crate) fn newest_session() -> Option<SessionRef> {
    global_transcripts(TOP_LEVEL_DEPTH)
        .into_iter()
        .filter_map(|p| Some((mtime_of(&p)?, session_id_from_path(&p)?, p)))
        .max_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        })
        .map(|(updated, id, path)| SessionRef { id, path, updated })
}

/// The workspace one session was recorded in, located by transcript filename —
/// [`find_session`] plus [`SessionRef::workspace`] in one call, which is all the
/// `delegate` resume path needs. `None` covers both dead ends it reports as one:
/// no transcript of that id, and a transcript recording no workspace.
pub(crate) fn workspace_of(session_id: &str) -> Option<PathBuf> {
    find_session(session_id)?.workspace()
}

/// Every transcript a live isolated runtime is holding, at the same tier-1 cost
/// (filenames and mtimes, nothing opened).
///
/// These are never resolutions — a resume spawns against the shared store and
/// cannot read any of them. They exist to EXPLAIN what the shared store alone
/// cannot: a session `clauth sessions` just listed is not "no session found",
/// and the newest session on the machine going missing from `latest` is not a
/// reason to silently resume the second newest.
pub(crate) fn live_isolated_holds() -> Vec<IsolatedHold> {
    isolated_holds(WALK_MAX_DEPTH)
}

/// The same holds, cut to the transcripts a rescue could make resumable —
/// [`newest_session`]'s own depth, so the two sides of a `latest` comparison
/// range over the same kind of file. A nested transcript can never become
/// `latest`, so one being newer is no reason to refuse a resume.
pub(crate) fn live_isolated_top_level_holds() -> Vec<IsolatedHold> {
    isolated_holds(TOP_LEVEL_DEPTH)
}

fn isolated_holds(depth: usize) -> Vec<IsolatedHold> {
    let mut out = Vec::new();
    for (profile, projects) in crate::runtime::live_isolated_stores() {
        let mut paths = Vec::new();
        collect_jsonl(&projects, depth, &mut paths);
        for path in paths {
            let (Some(id), Some(updated)) = (session_id_from_path(&path), mtime_of(&path)) else {
                continue;
            };
            out.push(IsolatedHold {
                session: SessionRef { id, path, updated },
                profile: profile.clone(),
            });
        }
    }
    out
}

/// Annotate one session in place with its token total and API-equivalent cost —
/// the full-transcript parse [`build_index`] deliberately skips, so a caller pays
/// it only when it wants these figures. Idempotent; safe to re-run.
///
/// `tokens` is input+output summed across models (`ModelTokens::in_out` — the
/// "tokens used" basis the Tokens tab headlines; cache is excluded so a resume's
/// carried-forward cache reads don't inflate the figure). It stays `None` — never
/// `Some(0)` — when the file yields no token-bearing row, so a session with no
/// usage renders blank rather than a misleading zero.
///
/// `cost` sums each (model, day) pair's hourly buckets at that day's dated rate
/// ([`PriceTable::cost_day`]): `Some(usd)` when a table is present and at least
/// one pair has a matching rate; `None` when no table is given OR every pair is
/// unpriced. The priced/unpriced boundary is read from the rate table directly,
/// not from `usd > 0`, so a priced but genuinely zero-cost session is
/// `Some(0.0)` — distinct from an unpriced `None`.
pub(crate) fn annotate(info: &mut SessionInfo, price: Option<&PriceTable>) {
    let days = crate::tokens::file_hourly_model_tokens(&info.path);
    // >= 1 token-bearing row ⇒ a real total (possibly 0); no rows ⇒ blank.
    info.tokens = (!days.is_empty()).then(|| {
        days.iter()
            .map(|d| {
                d.hours
                    .iter()
                    .map(|h| h.input.saturating_add(h.output))
                    .sum::<u64>()
            })
            .sum()
    });
    info.cost = price.and_then(|p| {
        let mut usd = 0.0;
        let mut any_priced = false;
        for d in &days {
            if let Some(c) = p.cost_day(&d.model, &d.day, &d.hours) {
                usd += c;
                any_priced = true;
            }
        }
        // "At least one pair priced" is read off the table, not `usd > 0`, so a
        // priced zero-cost session reads `Some(0.0)` while all-unpriced reads None.
        any_priced.then_some(usd)
    });
}

/// Annotate every session across all groups (the CLI's eager pass; the TUI may
/// instead call [`annotate`] lazily per visible row).
pub(crate) fn annotate_all(groups: &mut [WorkspaceGroup], price: Option<&PriceTable>) {
    for group in groups.iter_mut() {
        for session in group.sessions.iter_mut() {
            annotate(session, price);
        }
    }
}

// ── A3: session → last-ran-profile store ─────────────────────────────────────
//
// A single GLOBAL file under `~/.clauth/` keyed by session id (NOT per-profile —
// a shared-store session is cross-profile, so its owner can't live under any one
// profile dir). Hand-rolled load/save against `clauth_dir()`, mirroring
// `pricing.rs` / `token_ledger.rs`, since the crate has no shared global-cache
// helper.

/// Global store filename under `~/.clauth/`.
const SESSION_PROFILES_FILE: &str = "session_profiles.json";

/// Which profile a session last ran under. A stored `Contested` is distinct from
/// absent: two different profiles have both touched the same shared-store
/// session, so the owner is genuinely unknown and must never resolve to either —
/// while an absent id is simply unobserved. Both read back as "unknown".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionOwner {
    Known(String),
    Contested,
}

/// The persisted store. A named wrapper (not a bare map) leaves room to add
/// fields later without breaking the on-disk shape, matching `token_ledger.rs`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionProfiles {
    /// session id → owner stamp.
    sessions: HashMap<String, SessionOwner>,
}

/// `~/.clauth/session_profiles.json`; `None` only when the home dir can't be
/// resolved.
fn store_path() -> Option<PathBuf> {
    clauth_dir().ok().map(|d| d.join(SESSION_PROFILES_FILE))
}

/// Load the store, or an empty one when absent/unreadable/corrupt — a missing
/// owner renders blank, never fatal.
fn load_store(path: &Path) -> SessionProfiles {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Persist the store atomically (0o600).
fn save_store(path: &Path, store: &SessionProfiles) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(store).map_err(std::io::Error::other)?;
    atomic_write_600(path, &bytes)
}

/// Fold one observed session id under `profile` into the owner map. Absent or
/// already `Known(profile)` ⇒ `Known(profile)`; a different owner or a prior
/// `Contested` ⇒ `Contested`. Two profiles touching one shared session means the
/// owner can't be attributed, so it stays unknown rather than guessing the last
/// writer.
fn fold_owner(map: &mut HashMap<String, SessionOwner>, id: &str, profile: &str) {
    match map.entry(id.to_owned()) {
        Entry::Vacant(e) => {
            e.insert(SessionOwner::Known(profile.to_owned()));
        }
        Entry::Occupied(mut e) => {
            let contest = match e.get() {
                SessionOwner::Known(p) => p != profile,
                SessionOwner::Contested => true,
            };
            if contest {
                e.insert(SessionOwner::Contested);
            }
            // else: already ours — leave Known(profile) as-is.
        }
    }
}

/// The flock-POLL wait the hook's exact-owner stamp tolerates before it
/// degrades and skips the write. It bounds only the cross-process flock poll
/// ([`crate::lock::StateLock::acquire_with_timeout`]); the in-process
/// `THREAD_LOCK` wait that precedes the poll is unbounded. The hook manifest
/// gives the caller a 10 s host timeout, so this must sit well under it; a
/// hook must not block a tool call on a lock, which is why
/// `hook_note::ScopeLock` degrades on its own 2 s wait for the same reason.
const STAMP_STATE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// Land the hook's exact per-conversation attribution into the durable owner
/// store. Unlike [`fold_owner`], this SETS `Known(profile)` unconditionally:
/// the hook's answer is exact, so a prior `Contested` (or a differing sweep
/// guess) must never survive it. Called only for the MAIN conversation scope,
/// and only when the resolution first set or changed the account, so the
/// state flock is taken only on a real attribution, never every hook fire.
///
/// The flock poll is bounded well under the hook's host timeout (the in-process
/// `THREAD_LOCK` wait before it is not): a hook must not block a tool call on a
/// lock, so a contended state lock skips the stamp rather than stalling the
/// note.
pub(crate) fn stamp_exact_owner(session_id: &str, profile: &str) {
    let held = match crate::lock::StateLock::acquire_with_timeout(STAMP_STATE_LOCK_WAIT) {
        Ok(held) => held,
        Err(e) => {
            crate::logline::to_logfile(format_args!(
                "clauth: skipping the exact session owner stamp: {e}"
            ));
            return;
        }
    };
    let result = (|| {
        let Some(path) = store_path() else {
            return Ok(());
        };
        let mut store = load_store(&path);
        store.sessions.insert(
            session_id.to_owned(),
            SessionOwner::Known(profile.to_owned()),
        );
        save_store(&path, &store)
    })();
    drop(held);
    if let Err(e) = result {
        crate::logline::to_logfile(format_args!(
            "clauth: failed to stamp exact session owner: {e}"
        ));
    }
}

/// Session ids this run owns, from the walk's paths: filtered to this run's
/// window on a shared (cross-profile) store. An isolated store is exclusive to
/// the profile, so every file counts regardless of mtime.
fn run_session_ids(paths: &[PathBuf], isolated: bool, run_start: SystemTime) -> Vec<String> {
    paths
        .iter()
        .filter(|p| isolated || touched_since(p.as_path(), run_start))
        .filter_map(|p| session_id_from_path(p.as_path()))
        .collect()
}

/// Whether `path`'s mtime is at or after `since`. Fail-soft: an unreadable mtime
/// counts as outside the window (not this run's), so it is left unstamped.
fn touched_since(path: &Path, since: SystemTime) -> bool {
    mtime_of(path).is_some_and(|mtime| mtime >= since)
}

/// Record which sessions a `clauth start` run owned into the global store.
///
/// `projects_dir` is where the run's transcripts landed: an isolated runtime's
/// exclusive `runtime-isolated/projects/` (`isolated = true` — every file maps to
/// `profile`), or the shared global `~/.claude/projects/` (`isolated = false` —
/// only files touched at or after `run_start` are attributed, catching new and
/// resumed-during-this-run sessions without claiming another profile's untouched
/// ones).
///
/// Ids the exact per-conversation writer attributed are skipped outright
/// (owner ruling 2026-09-02): the hook record IS their attribution, and a
/// sweep fold could only contest it or stamp a rival account beside it. The
/// mtime sweep stays for ids the exact writer never saw — subagent
/// transcripts, hook-less conversations, and ids it saw but never
/// attributed. Read [`owner_of`] for the skip's other half.
///
/// A shared run also prunes the owner store in the same pass — the destructive
/// half. It drops every owner whose id has no transcript in the walked global
/// tree, refused on two guards ([`prepare_prune`]): an incomplete walk (some
/// subtree unreadable, the depth cap truncated, or a symlinked dir skipped) and
/// an empty walk (never read as "no transcripts exist") both skip the prune
/// rather than bulk-reap.
///
/// The keep-set and refusal checks are computed above the state flock; they
/// read nothing it protects, and a session going live between the read and the
/// prune only widens the keep-set — the safe direction. The read-modify-write
/// then runs under the flock so two concurrent `clauth start` runs retain, fold
/// their stamps, and save serially instead of clobbering each other. Best-effort
/// throughout: the session already ran, so any IO error is logged and swallowed
/// — never propagated to fail `start`.
pub(crate) fn stamp_run_sessions(
    profile: &str,
    projects_dir: &Path,
    isolated: bool,
    run_start: SystemTime,
) {
    let mut paths = Vec::new();
    let walk_complete = collect_jsonl(projects_dir, WALK_MAX_DEPTH, &mut paths);
    let ids: Vec<String> = run_session_ids(&paths, isolated, run_start)
        .into_iter()
        .filter(|id| crate::hook_note::resolved_account(id).is_none())
        .collect();

    // An isolated run never prunes and, with no ids to fold, has nothing to do
    // under the state flock; skip the acquisition.
    if isolated && ids.is_empty() {
        return;
    }

    // The prune runs on a shared run only. An isolated run's walk is its own
    // throwaway tree, not the global tree the prune keys on, and the owner
    // store deliberately holds ids whose transcripts live only in isolated
    // stores — exactly the population this prune must not reap.
    let prune_keep = if isolated {
        None
    } else {
        prepare_prune(&paths, walk_complete)
    };

    let result = crate::lock::with_state_lock(|_held| {
        let Some(path) = store_path() else {
            return Ok(());
        };
        let mut store = load_store(&path);
        let mut changed = false;

        if let Some(keep) = &prune_keep {
            changed = prune_owner_store_with_inputs(&mut store, keep);
        }

        if !ids.is_empty() {
            for id in &ids {
                fold_owner(&mut store.sessions, id, profile);
            }
            changed = true;
        }

        if changed {
            save_store(&path, &store)?;
        }
        Ok(())
    });
    if let Err(e) = result {
        logline!("clauth: failed to stamp session owners: {e}");
    }
}

/// The prune's keep-set, computed above the state flock. Everything the
/// retention needs except the per-id grace read, which must stay late: a fire
/// landing after a stale grace read would reap a live record, the wrong
/// direction, while a session going live after a stale isolated read only
/// widens the keep-set.
///
/// Two populations are kept even when the walked global tree has no transcript
/// for them: an id whose transcript lives only in a live isolated store
/// ([`live_isolated_holds`]), and an id whose main-scope record fired within
/// the hook's missing-transcript grace — a SessionStart stamped before Claude
/// Code wrote the transcript file must survive the same way the record sweep
/// keeps that record.
struct PruneKeep {
    live: HashSet<String>,
    isolated_ids: HashSet<String>,
}

/// The refusal checks plus the keep-set, or `None` when the prune is refused.
///
/// Two walk guards are load-bearing. An INCOMPLETE walk — some directory below
/// the root could not be read, the depth cap truncated a subtree, or a
/// symlinked dir hid one — is refused: [`collect_jsonl`] reports the partial
/// failure, and pruning on a walk that silently missed a subtree is a bulk
/// reap. An EMPTY walk is refused, never treated as "no transcripts exist": a
/// genuinely empty global store and a walk that saw nothing answer the same,
/// and pruning either wipes the store.
///
/// The walk is the FULL [`WALK_MAX_DEPTH`], not [`TOP_LEVEL_DEPTH`]: nested
/// per-session trees (`subagents/`, `workflows/`, `tool-results/`) hold real
/// transcripts deeper than the resume-visible depth, and pruning on the shallow
/// walk would reap their owners too.
fn prepare_prune(paths: &[PathBuf], walk_complete: bool) -> Option<PruneKeep> {
    if !walk_complete {
        logline!(
            "clauth: refusing to prune session owners: the global transcript walk was incomplete"
        );
        return None;
    }
    if paths.is_empty() {
        logline!(
            "clauth: refusing to prune session owners: the global transcript walk returned nothing"
        );
        return None;
    }
    let live: HashSet<String> = paths
        .iter()
        .filter_map(|p| session_id_from_path(p))
        .collect();
    let isolated_ids: HashSet<String> = live_isolated_holds()
        .into_iter()
        .map(|hold| hold.session.id)
        .collect();
    Some(PruneKeep { live, isolated_ids })
}

/// Retain only kept owners, in place on a pre-loaded store. Returns whether the
/// store changed. Shared runs only: the caller skips this on an isolated run,
/// whose walk is the isolated throwaway tree rather than the global tree the
/// prune keys on, and whose owner-store entries are exactly the isolated
/// transcripts this prune must not reap.
fn prune_owner_store_with_inputs(store: &mut SessionProfiles, keep: &PruneKeep) -> bool {
    let before = store.sessions.len();
    store.sessions.retain(|id, _| {
        keep.live.contains(id)
            || keep.isolated_ids.contains(id)
            || crate::hook_note::last_fire_within_missing_transcript_grace(id)
    });
    store.sessions.len() != before
}

/// Testable wrapper: prepare the keep-set, then retain.
/// [`stamp_run_sessions`] splits these so the prepare runs above the state
/// flock and only the retain stays inside it.
#[cfg(test)]
fn prune_owner_store(store: &mut SessionProfiles, paths: &[PathBuf], walk_complete: bool) -> bool {
    let Some(keep) = prepare_prune(paths, walk_complete) else {
        return false;
    };
    prune_owner_store_with_inputs(store, &keep)
}

/// One session's owner, or `None` when it is absent or `Contested` — both mean
/// unknown, and a `Contested` id must never resolve to either contender.
fn owner_in(store: &SessionProfiles, session_id: &str) -> Option<String> {
    match store.sessions.get(session_id)? {
        SessionOwner::Known(p) => Some(p.clone()),
        SessionOwner::Contested => None,
    }
}

/// The profile one session last ran under — the single-id counterpart to
/// [`annotate_owners`], for a caller holding an id rather than an index.
pub(crate) fn owner_of(session_id: &str) -> Option<String> {
    crate::hook_note::resolved_account(session_id)
        .or_else(|| owner_in(&load_store(&store_path()?), session_id))
}

/// Annotate each session's `last_ran_profile`. The exact per-conversation
/// observation wins where it exists; the global owner store answers for ids it
/// never saw. Loads the store once, so a caller can attach owners without
/// paying the per-session full-transcript parse [`annotate`] costs. Leaves
/// `None` for a session that is absent or `Contested` (both mean "unknown").
pub(crate) fn annotate_owners(groups: &mut [WorkspaceGroup]) {
    let Some(path) = store_path() else {
        return;
    };
    let store = load_store(&path);
    for group in groups.iter_mut() {
        for session in group.sessions.iter_mut() {
            session.last_ran_profile = crate::hook_note::resolved_account(&session.id)
                .or_else(|| owner_in(&store, &session.id));
        }
    }
}

// ── Session rescue: lift an isolated transcript into the global store ─────────
//
// An isolated runtime is GC'd along with its throwaway `projects/` store, which
// would take any session that ran under it. Rescue copies the transcript into
// the shared global store so it outlives that GC. Data safety is the one hard
// rule: copy, verify the copy landed intact, only THEN drop the source — a crash
// at any point leaves at worst a duplicate (source + target), never a loss.

/// Move `src` to `dst` without ever destroying `src` before the copy is proven
/// intact. Copies into a temp sibling of `dst`, fsyncs it, renames it into place
/// (atomic on the same filesystem), reads the landed file back to compare it
/// byte-for-byte, and removes `src` only once that verify passes. A verify
/// mismatch returns an error with `src` left in place.
pub(crate) fn rescue_move(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Same path: nothing to move, and a rename-over-self would destroy the file.
    if src == dst {
        return Ok(());
    }

    // A transcript is a bounded JSONL and rescue is rare, so a full read is cheap
    // and lets the post-rename verify compare the landed bytes against these.
    let bytes = std::fs::read(src)?;

    // Owner-only from birth, matching the files landing inside it: `~/.claude/`
    // is world-traversable, so a plain `create_dir_all` would leave a rescued
    // `sessions/`, `paste-cache/`, etc. at the process umask (typically 0755),
    // letting another local user list session ids even though the files
    // themselves stay 0600. Birth only, not a retighten: a dir this call finds
    // already on disk (e.g. left loose by a pre-fix build) keeps its existing
    // mode, same as `enforce_clauth_perms`'s own no-op-on-existing behavior —
    // and that retighten walk is scoped to `~/.clauth` only, deliberately never
    // `~/.claude`, which clauth does not own outright.
    if let Some(parent) = dst.parent() {
        mkdir_700(parent)?;
    }
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let file_name = dst
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    let tmp = dir.join(format!(".{file_name}.rescue.tmp.{}", std::process::id()));
    // Clear any stale temp from a crashed prior rescue so `create` lands clean.
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    {
        // Local import: a module-level `Write` collides with the `Read::by_ref`
        // the tail reader above relies on.
        use std::io::Write;
        // Owner-only from birth on unix. The temp lives in the DESTINATION dir
        // and `~/.claude` is world-traversable, so `File::create`'s
        // umask-masked 0644 would expose the bytes for the whole write window —
        // CC writes transcripts and paste-cache entries 0600, and narrowing
        // after the write loses the race against anyone holding the fd open.
        #[cfg(unix)]
        let mut f = {
            use std::fs::OpenOptions;
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        // Then take the source's own mode, so an entry CC writes executable
        // stays executable. Best-effort: a filesystem that refuses it leaves the
        // stricter 0600, which must not turn a rescue into a discard.
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(src) {
            let _ = f.set_permissions(meta.permissions());
        }
        // Durable before the rename so a crash can't promote a torn temp to dst.
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Verify the copy landed intact before the source is removed. On any
    // mismatch, return early — `src` is still on disk, so nothing is lost.
    let landed = std::fs::read(dst)?;
    if landed != bytes {
        return Err(std::io::Error::other(format!(
            "rescue verify failed: {} does not match source {}",
            dst.display(),
            src.display()
        )));
    }

    std::fs::remove_file(src)?;
    Ok(())
}

/// Rescue an isolated session transcript at `src` into the global store,
/// preserving its `<slug>` subdir so `--resume` run from that workspace still
/// finds it. Collision-safe on the final `<id>.jsonl`: a byte-identical target
/// is already-rescued (source dropped, no duplicate); a differing target is a
/// real id collision with another session and is never overwritten — the rescue
/// lands beside it as `<id>.rescued-<n>.jsonl`. Returns the final path.
pub(crate) fn rescue_session_transcript(
    src: &Path,
    iso_projects_root: &Path,
    global_projects_root: &Path,
) -> std::io::Result<PathBuf> {
    // The isolated store mirrors the global `<slug>/<id>.jsonl` layout, so the
    // existing subdir is authoritative — preserve it verbatim rather than
    // recomputing the (lossy) slug from cwd.
    let rel = src.strip_prefix(iso_projects_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not under the isolated projects root {}",
                src.display(),
                iso_projects_root.display()
            ),
        )
    })?;
    let target = global_projects_root.join(rel);
    rescue_file(src, &target)
}

/// Move `src` onto `target`, never overwriting an occupied `target`. A
/// byte-identical target means the entry is already rescued, so the source is
/// dropped and the single copy kept; anything else is a real name collision and
/// the rescue lands beside it as the first free `<stem>.rescued-<n>[.<ext>]`.
/// Returns the final path.
fn rescue_file(src: &Path, target: &Path) -> std::io::Result<PathBuf> {
    if !target.exists() {
        rescue_move(src, target)?;
        return Ok(target.to_path_buf());
    }
    // `is_dir` guard: a directory sitting where the rescue wants a file is a name
    // collision like any other, and `files_equal` would only error on it.
    if !target.is_dir() && files_equal(src, target)? {
        std::fs::remove_file(src)?;
        return Ok(target.to_path_buf());
    }
    let sibling = free_rescued_sibling(target)?;
    rescue_move(src, &sibling)?;
    Ok(sibling)
}

/// Rescue every `*.jsonl` under an isolated run's `iso_projects` into the global
/// `global_projects` store, returning the count moved. Fail-soft per file: a
/// rescue error is logged and skipped so one bad transcript never blocks the
/// rest — the isolated store is discarded right after this call, so a skipped
/// file is at worst a lost rescue, never corruption. Each move is collision- and
/// crash-safe (see [`rescue_session_transcript`]).
pub(crate) fn rescue_isolated_store(iso_projects: &Path, global_projects: &Path) -> usize {
    let mut paths = Vec::new();
    collect_jsonl(iso_projects, WALK_MAX_DEPTH, &mut paths);
    let mut moved = 0usize;
    for src in paths {
        match rescue_session_transcript(&src, iso_projects, global_projects) {
            Ok(_) => moved += 1,
            Err(e) => logline!("clauth: failed to rescue {}: {e}", src.display()),
        }
    }
    moved
}

/// The per-session sidecar trees a rescue lifts out of an isolated runtime.
/// An ALLOWLIST, applied to what the store actually holds: the walk enumerates
/// the runtime root and moves the ones present, so a Claude Code release that
/// renames a tree shows up as "not rescued" (and in the untouched-entry log
/// line) rather than as a blind path that silently misses.
///
/// The bar for a name here is "a rescued session needs it to resume", which is
/// NOT the same as "CC wrote it". Everything in an isolated tree is CC-authored
/// (it links nothing from `~/.claude`), and plenty of that is not session
/// state and must never reach the operator's store: `security/` holds the
/// hundreds-of-MB venv `/security-review` builds, `daemon/` holds CC's 0600
/// `control.key`, `backups/` holds verbatim `.claude.json` snapshots, and
/// `statsig|ide|debug|telemetry` are machine-scoped caches. None of them is on
/// this list, so none is ever a candidate. `projects/` is absent too: the
/// transcript leg moves it under its own slug mapping.
///
/// Enumerated against CC 2.1.215 — the release that has NO `todos/` left, which
/// is why the list is checked against the disk rather than trusted blind.
/// Re-check it when CC's config-dir layout moves.
const SIDECAR_TREES: &[&str] = &[
    "file-history",
    "paste-cache",
    "plans",
    "session-env",
    "sessions",
    "shell-snapshots",
    "tasks",
    "todos",
];

/// Recursion cap for the sidecar merge, counted from the runtime root so a
/// top-level tree is depth 1. CC's own trees nest two or three
/// (`file-history/<session>/<entry>`); the cap bounds a pathological one, and
/// hitting it is logged — a truncated subtree is state left in a tree that is
/// about to be discarded.
const SIDECAR_MAX_DEPTH: usize = 8;

/// Whether a top-level isolated-runtime entry is session sidecar state to
/// rescue. Name-only: with an allowlist the entry's shape carries no safety
/// weight (the old "directories only" rule existed to bound what a denylist let
/// through), so the caller merges a dir and moves a file under the same name.
fn rescuable_sidecar(name: &std::ffi::OsStr) -> bool {
    SIDECAR_TREES.iter().any(|tree| name == *tree)
}

/// Rescue Claude Code's session sidecar state out of an isolated runtime root
/// into the global `~/.claude/`, returning how many files ended up there (an
/// already-present identical copy counts, matching the transcript leg).
/// [`SIDECAR_TREES`] is the admission rule; every other entry is left for the
/// GC and named once in a log line, so a renamed tree is visible.
///
/// The merge is per ENTRY, never per tree: the global store usually already has
/// a `shell-snapshots/`, so moving the dir wholesale would replace it. Each
/// moved file keeps the transcript leg's collision safety ([`rescue_file`]).
/// Fail-soft per entry like that leg — a failure is logged and skipped, since
/// the isolated tree is discarded right after this call.
///
/// Symlinks are skipped on BOTH sides, never followed: an isolated runtime links
/// nothing, so a source link is anomalous and walking one could reach the
/// operator's own store, while `~/.claude` does hold operator links pointing
/// outside the store (`skills -> ~/.agents/…`) that a rescue must not write
/// through.
pub(crate) fn rescue_isolated_sidecars(iso_root: &Path, global_root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(iso_root) else {
        return 0;
    };
    let mut moved = 0usize;
    let mut untouched: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !rescuable_sidecar(&name) {
            if entry.file_type().is_ok_and(|t| t.is_dir()) && name != "projects" {
                untouched.push(name.to_string_lossy().into_owned());
            }
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let src = entry.path();
        let dst = global_root.join(&name);
        if is_symlink(&dst) {
            logline!(
                "clauth: skipping rescue of {}, {} is a symlink",
                src.display(),
                dst.display()
            );
            continue;
        }
        if file_type.is_dir() {
            moved += rescue_tree(&src, &dst, SIDECAR_MAX_DEPTH - 1);
        } else {
            match rescue_file(&src, &dst) {
                Ok(_) => moved += 1,
                Err(e) => logline!("clauth: failed to rescue {}: {e}", src.display()),
            }
        }
    }
    if !untouched.is_empty() {
        untouched.sort();
        logline!(
            "clauth: left {} in the isolated store (not session state)",
            untouched.join(", ")
        );
    }
    moved
}

/// Merge one isolated sidecar tree into its global counterpart entry by entry,
/// returning the file count moved. Symlinks are skipped rather than followed on
/// both sides (so no link cycle is entered and no write escapes the store).
/// Reaching the depth cap is logged, never silent: what it drops is state in a
/// tree about to be discarded.
fn rescue_tree(src: &Path, dst: &Path, depth: usize) -> usize {
    if depth == 0 {
        logline!(
            "clauth: rescue depth cap reached at {}, leaving the subtree",
            src.display()
        );
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(src) else {
        return 0;
    };
    let mut moved = 0usize;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if is_symlink(&target) {
            logline!(
                "clauth: skipping rescue of {}, {} is a symlink",
                path.display(),
                target.display()
            );
            continue;
        }
        if file_type.is_dir() {
            moved += rescue_tree(&path, &target, depth - 1);
            continue;
        }
        match rescue_file(&path, &target) {
            Ok(_) => moved += 1,
            Err(e) => logline!("clauth: failed to rescue {}: {e}", path.display()),
        }
    }
    moved
}

/// Whether `path` is a symlink itself (never following it). A missing path is
/// not one.
fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether two files hold identical bytes. Length is checked first as a cheap
/// reject before reading both.
fn files_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// The first free `<stem>.rescued-<n>[.<ext>]` sibling of `target`, smallest
/// `n`. A sidecar file may carry no extension, which stays extension-less
/// rather than gaining an invented one.
fn free_rescued_sibling(target: &Path) -> std::io::Result<PathBuf> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("target {} has no usable file stem", target.display()),
        )
    })?;
    let ext = target.extension().and_then(|e| e.to_str());
    for n in 0u32..u32::MAX {
        let candidate = dir.join(match ext {
            Some(ext) => format!("{stem}.rescued-{n}.{ext}"),
            None => format!("{stem}.rescued-{n}"),
        });
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free rescued sibling for {}", target.display()),
    ))
}

#[cfg(test)]
#[path = "../tests/inline/sessions.rs"]
mod tests;
