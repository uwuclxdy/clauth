//! Session index over Claude Code transcript stores.
//!
//! Builds a newest-first, workspace-grouped inventory of CC sessions across the
//! global `~/.claude/projects/` store plus every live isolated runtime's own
//! store. Two tiers, each surface paying only its own: [`walk`] lists every
//! transcript from filenames and mtimes alone (nothing opened), and
//! [`preview`] reads ONE transcript's bounded HEAD and seek-from-end TAIL for
//! its workspace and message previews — never a full-transcript parse, which
//! the token subsystem already shows is too heavy to run per index build.
//! [`build_index`] previews the whole walk; a page previews only its rows,
//! through [`preview_labelled`], whose one tail walk also finds the [`Labels`].
//! [`read_page`] serves one transcript's records backward from a byte cursor
//! through the same backward line walker the tail preview uses.
//!
//! The `tokens`/`cost` annotation ([`annotate`]) and the `last_ran_profile`
//! stamp ([`annotate_owners`]) are separate passes over the rows a caller
//! already holds; a row missing from either renders blank, never `0`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::logline::logline;
use crate::pricing::PriceTable;
use crate::profile::{atomic_write_600, claude_dir, clauth_dir, mkdir_700};

// Test-only ledger of transcript opens, so `clauth sessions --live` can prove
// it opens none: every preview/history transcript read in this module bumps
// it, and the `--live` test asserts the counter stays put. Rescue/move reads
// and opens through `crate::tokens` / `crate::tps` are not counted.
// Thread-local so parallel `cargo test` threads never share a count.
#[cfg(test)]
thread_local! {
    static TRANSCRIPT_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn transcript_opens() -> usize {
    TRANSCRIPT_OPENS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn reset_transcript_opens() {
    TRANSCRIPT_OPENS.with(|c| c.set(0));
}

#[cfg(test)]
fn bump_transcript_open() {
    TRANSCRIPT_OPENS.with(|c| c.set(c.get() + 1));
}

/// Bytes read from a file's head to recover its workspace and first user
/// message. The session id comes from the filename stem, not the head, so this
/// window only has to reach the first user turn, which sits at or near the top.
const HEAD_MAX_BYTES: u64 = 256 * 1024;
/// Initial tail-read window scanned backward for the last user message.
const TAIL_CHUNK: u64 = 64 * 1024;
/// Ceiling the tail window grows to while a chunk lacks what the walk wants (a
/// user line, or a row's [`Labels`]), bounding the read on a transcript whose
/// tail is all tool traffic or that never named itself.
const TAIL_MAX: u64 = 1024 * 1024;
/// Byte budget of one history page ([`read_page`]): the page stops adding
/// older records once the next one would push it past this, except that an
/// empty page takes that one record whatever its size, so no record is ever
/// truncated or unreachable.
pub(crate) const PAGE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Preview length cap, in terminal cells and characters (not bytes — the cut lands between
/// graphemes, so a two-cell glyph is never split).
const PREVIEW_MAX_CELLS: usize = 200;
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

/// One indexed session: a [`Walked`] entry plus its previews.
///
/// `tokens`, `cost`, and `last_ran_profile` are populated by separate passes
/// ([`annotate`], [`annotate_owners`]) and are `None` off [`preview`]. They
/// stay `Option` on purpose: a session missing from the token stats or the
/// last-ran map renders blank, never `0`/empty-string.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    /// The session id — the transcript filename stem (`<sessionId>.jsonl`), the
    /// id `claude --resume <id>` resolves by. NOT the in-file `sessionId`, which
    /// a resume copy carries forward from its parent. Deliberately not redacted
    /// (a filename stem: a uuid, or a nested transcript's `agent-…`); only the
    /// message previews below are.
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
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    message: Option<Message>,
    #[serde(rename = "agentSetting")]
    agent_setting: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
}

impl TranscriptLine {
    fn is_user(&self) -> bool {
        self.message
            .as_ref()
            .is_some_and(|msg| msg.role.as_deref() == Some("user"))
    }
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
    if !line.is_user() {
        return None;
    }
    let raw = match line.message.as_ref()?.content.as_ref()? {
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

/// Redact then truncate to a bounded preview measured in terminal cells and
/// characters.
/// Redaction runs on the full text first so a secret can never survive by
/// straddling the truncation point. Each character `terminal_safe`'s predicate
/// set drops — a control character or a display-reordering one — becomes one
/// space before the cut, so a line boundary mid-text stays visible; the
/// trailing `trim` drops such a space at either end, and neither an escape
/// sequence nor a bidi override reaches the terminal.
fn preview_of(raw: &str) -> Option<String> {
    let redacted = redact_secrets(raw.trim());
    let sanitized = redacted
        .chars()
        .map(|c| {
            if crate::format::unsafe_on_terminal(c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(crate::format::truncate(trimmed, PREVIEW_MAX_CELLS))
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
    #[cfg(test)]
    bump_transcript_open();
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

/// The last `agent-setting` record's agent name over the WHOLE transcript.
///
/// The record can sit anywhere: Claude Code writes it when an agent is set or
/// restored, never per turn, so a session that took its agent at a later resume
/// carries its only record past any bounded head window. The scan therefore
/// streams the whole file and parses only the lines whose text carries the
/// record kind (a substring prefilter keeps a multi-megabyte transcript cheap).
/// A non-UTF-8 line is one bad record, not the end of the file: it is skipped,
/// as is an unparseable one.
fn read_last_agent(path: &Path) -> Option<String> {
    #[cfg(test)]
    bump_transcript_open();
    let Ok(file) = File::open(path) else {
        return None;
    };
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(_) => break,
        };
        if !line.contains("\"agent-setting\"") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) else {
            continue;
        };
        if let Some(a) = parsed
            .agent_setting
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            last = Some(a.to_owned());
        }
    }
    last
}

/// The last user message, found by walking the tail's lines backward — never a
/// full parse.
#[cfg(test)]
fn read_last_user_message(path: &Path) -> Option<String> {
    read_tail(path, TailWant::LastMessage).last_message
}

/// A session's Claude Code name and git branch, off the tail walk that finds
/// its last user message.
#[derive(Debug, Clone, Default)]
pub(crate) struct Labels {
    /// The newest `custom-title` record's name, sanitized like a preview;
    /// `None` when no such record lies within [`TAIL_MAX`] of the end, or the
    /// newest one sanitizes to nothing.
    pub(crate) title: Option<String>,
    /// The `gitBranch` of the newest user record within [`TAIL_MAX`] of the
    /// end that names one, with control and bidi characters dropped; `None`
    /// when no record there names one that survives the drop.
    pub(crate) git_branch: Option<String>,
}

/// What a tail walk stops for: the last user message alone, or that plus the
/// [`Labels`], which can sit further back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TailWant {
    LastMessage,
    WithLabels,
}

#[derive(Default)]
struct Tail {
    last_message: Option<String>,
    labels: Labels,
}

/// One backward walk of the tail's lines, stopping once it holds what `want`
/// names. The walk covers at most [`TAIL_MAX`] bytes from the end, so a field
/// whose record lies only further back stays `None` (a tail of pure tool
/// traffic has no last message). Fail-soft: any IO error yields every field
/// `None`.
fn read_tail(path: &Path, want: TailWant) -> Tail {
    let mut tail = Tail::default();
    let mut title_seen = false;
    let walked = lines_before(path, None, TAIL_WINDOW, |_, line| {
        let labels = &mut tail.labels;
        let Ok(text) = std::str::from_utf8(line) else {
            return ControlFlow::Continue(());
        };
        // Past the last message and branch only a `custom-title` record is
        // left to find, so every other line skips the parse.
        let title_only = tail.last_message.is_some() && labels.git_branch.is_some();
        if title_only && !text.contains("\"custom-title\"") {
            return ControlFlow::Continue(());
        }
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(text) else {
            return ControlFlow::Continue(());
        };
        if tail.last_message.is_none() {
            tail.last_message = user_text(&parsed);
        }
        if want == TailWant::WithLabels {
            if !title_seen && parsed.kind.as_deref() == Some("custom-title") {
                title_seen = true;
                labels.title = parsed.custom_title.as_deref().and_then(preview_of);
            }
            if labels.git_branch.is_none() && parsed.is_user() {
                labels.git_branch = parsed
                    .git_branch
                    .as_deref()
                    .map(crate::format::terminal_safe)
                    .filter(|branch| !branch.is_empty());
            }
        }
        let done = tail.last_message.is_some()
            && (want == TailWant::LastMessage || (title_seen && labels.git_branch.is_some()));
        if done {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    match walked {
        Ok(()) => tail,
        Err(_) => Tail::default(),
    }
}

/// How far a backward line walk reads: its first window, doubled whenever a
/// window holds no whole line, and the floor below the walk's end it never
/// reads past.
#[derive(Clone, Copy)]
struct Window {
    first: u64,
    max_bytes: u64,
}

/// The tail preview's walk: bounded, so a tail of pure tool traffic costs at
/// most [`TAIL_MAX`].
const TAIL_WINDOW: Window = Window {
    first: TAIL_CHUNK,
    max_bytes: TAIL_MAX,
};

/// The history page's walk: unbounded below, because a record is served whole
/// whatever its size (the page budget is [`PAGE_MAX_BYTES`], applied by the
/// visitor); the walk still reads one window at a time and stops the moment
/// the visitor has its page.
const PAGE_WINDOW: Window = Window {
    first: TAIL_CHUNK,
    max_bytes: u64::MAX,
};

/// Visit the whole lines of `path` whose bytes end at or before `end` (the
/// file's end when `None`), newest first, each with the byte offset it
/// starts at, until `visit` breaks or the walk reaches byte 0 or its floor
/// (`end - max_bytes`).
///
/// One window at a time, seeking backward: a window's first line is dropped
/// as a cut unless the window starts at byte 0 (the next window ends right
/// after that line so it is visited whole), and a window holding no whole
/// line doubles. A line straddling the floor is never visited, and neither is
/// the head of a line an `end` inside it cut: that line ends past `end`, so
/// it is nobody's. The terminator of the last line is not a line; an empty
/// line elsewhere is one, so a blank line reaches the visitor. `Err` when the
/// file cannot be opened or read; the lines visited before a mid-walk error
/// stand.
fn lines_before(
    path: &Path,
    end: Option<u64>,
    window: Window,
    mut visit: impl FnMut(u64, &[u8]) -> ControlFlow<()>,
) -> std::io::Result<()> {
    #[cfg(test)]
    bump_transcript_open();
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut end = end.map_or(len, |end| end.min(len));
    let floor = end.saturating_sub(window.max_bytes);
    let mut size = window.first.max(1);
    let mut buf = Vec::new();
    while end > floor {
        let start = end.saturating_sub(size).max(floor);
        let read_len = end - start;
        file.seek(SeekFrom::Start(start))?;
        buf.clear();
        file.by_ref().take(read_len).read_to_end(&mut buf)?;
        if buf.len() as u64 != read_len {
            // The file shrank under the walk: whatever came before is gone.
            return Ok(());
        }

        let whole_from = if start == 0 {
            0
        } else {
            match buf.iter().position(|&b| b == b'\n') {
                Some(cut) => cut + 1,
                None => buf.len(),
            }
        };
        let region = &buf[whole_from..];
        if region.is_empty() {
            if start <= floor {
                return Ok(());
            }
            size = size.saturating_mul(2);
            continue;
        }

        let base = start + whole_from as u64;
        // The newest segment is not a line when it is the last line's
        // terminator, or the head of a line that `end` cut inside (only the
        // first window can end anywhere but after a newline or at the file's
        // end); every later window ends right after a newline.
        let skip_newest = region.last() == Some(&b'\n') || end < len;
        let mut seg_end = region.len();
        for (i, seg) in region.rsplit(|&b| b == b'\n').enumerate() {
            let seg_start = seg_end - seg.len();
            if !(i == 0 && skip_newest) && visit(base + seg_start as u64, seg).is_break() {
                return Ok(());
            }
            seg_end = seg_start.saturating_sub(1);
        }
        end = base;
        size = size.saturating_mul(2);
    }
    Ok(())
}

/// One page of a transcript's records, oldest first, as [`read_page`] serves
/// it.
pub(crate) struct Page {
    /// `(start offset, record)` per JSON-object line, in file order.
    pub(crate) records: Vec<(u64, serde_json::Value)>,
    /// The cursor for the page of older records, `None` when nothing older
    /// remains: every line below the newest one served was consumed.
    pub(crate) next_before: Option<u64>,
    /// Lines in the consumed range that were not a JSON object: a torn last
    /// line mid-write, a blank line.
    pub(crate) malformed: u32,
}

/// The last `limit` records of `path` ending at or before `before` (the file's
/// end when `None`), each a JSONL line parsed as a JSON object and nothing
/// more, within a `max_bytes` budget an empty page may exceed by its one
/// record. A line that is not a JSON object is skipped and counted. The walk
/// reads only what the page consumes, one window at a time.
pub(crate) fn read_page(
    path: &Path,
    before: Option<u64>,
    limit: usize,
    max_bytes: u64,
) -> std::io::Result<Page> {
    let mut records: Vec<(u64, serde_json::Value)> = Vec::new();
    let mut malformed = 0u32;
    let mut bytes = 0u64;
    let mut consumed_floor: Option<u64> = None;
    lines_before(path, before, PAGE_WINDOW, |offset, line| {
        let Some(record) = serde_json::from_slice::<serde_json::Value>(line)
            .ok()
            .filter(serde_json::Value::is_object)
        else {
            malformed += 1;
            consumed_floor = Some(offset);
            return ControlFlow::Continue(());
        };
        if !records.is_empty() && bytes.saturating_add(line.len() as u64) > max_bytes {
            return ControlFlow::Break(());
        }
        bytes = bytes.saturating_add(line.len() as u64);
        records.push((offset, record));
        consumed_floor = Some(offset);
        if records.len() >= limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    records.reverse();
    Ok(Page {
        records,
        next_before: consumed_floor.filter(|&offset| offset > 0),
        malformed,
    })
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

/// One transcript the walk located: its filename stem, mtime and store, with
/// nothing opened. [`preview`] turns it into a [`SessionInfo`] by reading the
/// head and tail — the per-row cost a page pays for its rows alone.
#[derive(Debug, Clone)]
pub(crate) struct Walked {
    /// The transcript filename stem, the same id [`SessionInfo::id`] carries.
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    /// File mtime, the listing's ordering key.
    pub(crate) updated: SystemTime,
    pub(crate) source: SessionSource,
}

impl Walked {
    /// The listing's sort key, for [`newest_first`].
    pub(crate) fn sort_key(&self) -> (SystemTime, &str) {
        (self.updated, &self.id)
    }
}

impl SessionInfo {
    /// The listing's sort key, for [`newest_first`].
    pub(crate) fn sort_key(&self) -> (SystemTime, &str) {
        (self.updated, &self.id)
    }
}

/// The listing order every surface shares: `updated` desc, then `id` asc, so
/// equal mtimes still order deterministically.
pub(crate) fn newest_first(a: (SystemTime, &str), b: (SystemTime, &str)) -> std::cmp::Ordering {
    b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1))
}

/// Read one located transcript's head and tail into its [`SessionInfo`]: the
/// workspace and first user message off the head, the last user message off
/// the tail. Best-effort: an unreadable file yields an empty workspace and no
/// previews rather than dropping the session, whose id came from the filename.
pub(crate) fn preview(entry: Walked) -> SessionInfo {
    preview_until(entry, TailWant::LastMessage).0
}

/// [`preview`] plus the row's [`Labels`], off the same tail walk; it reads on
/// past the last user message until it holds them or reaches [`TAIL_MAX`].
pub(crate) fn preview_labelled(entry: Walked) -> (SessionInfo, Labels) {
    preview_until(entry, TailWant::WithLabels)
}

fn preview_until(entry: Walked, want: TailWant) -> (SessionInfo, Labels) {
    let head = read_head(&entry.path);
    let tail = read_tail(&entry.path, want);
    let info = SessionInfo {
        id: entry.id,
        workspace: head.workspace,
        path: entry.path,
        updated: entry.updated,
        first_message: head.first_message,
        last_message: tail.last_message,
        source: entry.source,
        tokens: None,
        cost: None,
        last_ran_profile: None,
    };
    (info, tail.labels)
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

/// A workflow run's journal (`…/workflows/wf_<id>/journal.jsonl`): a run log,
/// not a conversation transcript, so it is no session a lookup may name.
pub(crate) fn is_workflow_journal(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "journal.jsonl")
        && path.parent().is_some_and(|parent| {
            parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("wf_"))
                && parent
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "workflows")
        })
}

/// Walk every `*.jsonl` under one store's `projects/` dir into `by_id`, keeping
/// the newest entry when a session id appears in more than one file. A file
/// with no usable stem, an unreadable mtime, or a workflow run's journal is
/// skipped.
fn walk_store(projects: &Path, source: &SessionSource, by_id: &mut HashMap<String, Walked>) {
    let mut paths = Vec::new();
    collect_jsonl(projects, WALK_MAX_DEPTH, &mut paths);
    for path in paths {
        if is_workflow_journal(&path) {
            continue;
        }
        let (Some(id), Some(updated)) = (session_id_from_path(&path), mtime_of(&path)) else {
            continue;
        };
        insert_newest(
            by_id,
            Walked {
                id,
                path,
                updated,
                source: source.clone(),
            },
        );
    }
}

/// Collapse a duplicate session id (the same `<id>.jsonl` copied into more than
/// one store or project-slug dir) to the newest by mtime. On an equal mtime the
/// lexicographically greater source path wins, so the pick stays stable
/// regardless of `read_dir` order.
fn insert_newest(map: &mut HashMap<String, Walked>, entry: Walked) {
    match map.entry(entry.id.clone()) {
        Entry::Occupied(mut e) => {
            let cur = e.get();
            let wins = entry.updated > cur.updated
                || (entry.updated == cur.updated && entry.path > cur.path);
            if wins {
                e.insert(entry);
            }
        }
        Entry::Vacant(e) => {
            e.insert(entry);
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
            sessions.sort_by(|a, b| newest_first(a.sort_key(), b.sort_key()));
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

/// Every transcript across the stores `clauth sessions` browses — the global
/// store plus every live isolated runtime's own — deduped by session id and
/// unsorted, with nothing opened: a `read_dir` walk and one stat per file.
/// Fail-soft throughout — an unreadable store is skipped, never fatal.
pub(crate) fn walk() -> Vec<Walked> {
    let mut by_id: HashMap<String, Walked> = HashMap::new();

    if let Ok(projects) = claude_dir().map(|d| d.join("projects")) {
        walk_store(&projects, &SessionSource::Global, &mut by_id);
    }
    for (profile, projects) in crate::runtime::live_isolated_stores() {
        walk_store(&projects, &SessionSource::Isolated { profile }, &mut by_id);
    }

    by_id.into_values().collect()
}

/// One session by exact id across the stores the listing browses: the file the
/// listing shows for that id. A lookup among the stems the walk yields, never a
/// path join, so an id spelled as a path (`../x`, `a/b`) can only miss.
pub(crate) fn locate(session_id: &str) -> Option<Walked> {
    walk().into_iter().find(|entry| entry.id == session_id)
}

/// Build the session index: every transcript the walk finds, previewed and
/// grouped by workspace, newest-first.
pub(crate) fn build_index() -> Vec<WorkspaceGroup> {
    group_by_workspace(walk().into_iter().map(preview).collect())
}

/// A file mtime as ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS+00:00`), the machine
/// shape `clauth sessions --json` and the sessions API share. A pre-epoch time
/// clamps to epoch 0.
pub(crate) fn updated_iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::usage::epoch_secs_to_iso(secs)
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

    /// The last `agent-setting` record's agent name over the whole transcript
    /// ([`read_last_agent`]): the agent a resume runs again.
    pub(crate) fn agent(&self) -> Option<String> {
        read_last_agent(&self.path)
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

/// Locate one session by exact id over the whole store, nested transcripts
/// included and workflow journals skipped — the lookup `clauth resume` and
/// `clauth info` resolve an id with (`clauth sessions` lists through
/// [`build_index`]). When the same id sits in more than one project-slug dir,
/// the newest mtime wins and an equal mtime falls back to the greater path —
/// [`insert_newest`]'s rule, so a targeted lookup and the index resolve one id
/// to the same file.
pub(crate) fn find_session(session_id: &str) -> Option<SessionRef> {
    find_session_within(WALK_MAX_DEPTH, session_id)
}

/// [`find_session`] limited to [`TOP_LEVEL_DEPTH`]: the transcripts Claude Code
/// can actually open for a resume value. A nested `agent-<hex>` stem is found
/// by the browsing lookup, but CC answers no-match for it (see
/// [`TOP_LEVEL_DEPTH`]), so a resume value that names one cannot be pinned and
/// its launch stays unreadable.
pub(crate) fn find_session_top_level(session_id: &str) -> Option<SessionRef> {
    find_session_within(TOP_LEVEL_DEPTH, session_id)
}

fn find_session_within(depth: usize, session_id: &str) -> Option<SessionRef> {
    let (updated, path) = global_transcripts(depth)
        .into_iter()
        .filter(|p| !is_workflow_journal(p))
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

/// Every top-level session id in the global store, sorted and deduplicated:
/// the ids a shell offers after `resume` and after `info`, beside `latest`. Only
/// [`TOP_LEVEL_DEPTH`], the depth [`newest_session`] resolves at: Claude Code
/// opens a top-level transcript alone, so a nested subagent transcript's
/// `agent-<hex>` stem never appears, and the depth also keeps every workflow
/// journal out. Filenames alone, so building the list opens no transcript.
pub(crate) fn resumable_session_ids() -> Vec<String> {
    let mut ids: Vec<String> = global_transcripts(TOP_LEVEL_DEPTH)
        .into_iter()
        .filter_map(|path| session_id_from_path(&path))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Every `*.jsonl` a live isolated runtime is holding, workflow journals
/// included, at the same tier-1 cost (filenames and mtimes, nothing opened).
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
/// own `runtime-isolated-<sid>/projects/` (`isolated = true` — every file maps to
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

/// Resolve the STORE half of the owner for a batch of session ids over ONE
/// owner-store load. The hook's `resolved` is read fresh by the caller, so this
/// answers only from `session_profiles.json`. `None` for an id that is absent
/// or `Contested`.
pub(crate) fn owners_of(ids: impl IntoIterator<Item = String>) -> HashMap<String, Option<String>> {
    let Some(path) = store_path() else {
        return HashMap::new();
    };
    let store = load_store(&path);
    ids.into_iter()
        .map(|id| {
            let owner = owner_in(&store, &id);
            (id, owner)
        })
        .collect()
}

/// Annotate each session's `last_ran_profile`. The exact per-conversation
/// observation wins where it exists; the global owner store answers for ids it
/// never saw. Loads the store once, so a caller can attach owners without
/// paying the per-session full-transcript parse [`annotate`] costs. Leaves
/// `None` for a session that is absent or `Contested` (both mean "unknown").
pub(crate) fn annotate_owners(groups: &mut [WorkspaceGroup]) {
    annotate_owners_of(
        groups
            .iter_mut()
            .flat_map(|group| group.sessions.iter_mut()),
    );
}

/// [`annotate_owners`] over any rows a caller holds — a page's, say.
pub(crate) fn annotate_owners_of<'a>(rows: impl IntoIterator<Item = &'a mut SessionInfo>) {
    let Some(path) = store_path() else {
        return;
    };
    let store = load_store(&path);
    for session in rows {
        session.last_ran_profile = crate::hook_note::resolved_account(&session.id)
            .or_else(|| owner_in(&store, &session.id));
    }
}

// ── Session rescue: lift a runtime tree's session state into the global store ─
//
// A dead runtime tree is removed with every real entry it holds: an isolated
// tree's throwaway `projects/`, and whatever a shared tree's session wrote that
// never reached `~/.claude`. Rescue carries its transcripts and sidecars into
// the global store first, so they outlive the removal. Data safety is the one
// hard rule: write, verify the global store holds the bytes intact, only THEN
// drop the source — a crash at any point leaves at worst a duplicate (source +
// target), never a loss.

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
    // already on disk keeps its existing mode — the retighten of a dir an older
    // build left loose is `load_config`'s `enforce_claude_dirs_perms`, scoped to
    // the dirs clauth's rescue and store-link writers create under `~/.claude`
    // (plus each direct child of `projects/`, CC's and clauth's alike), never
    // the whole tree.
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

/// What a rescue does with an entry whose name the global store already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Occupied {
    /// A byte-identical target means the entry is already rescued, so the
    /// source is dropped; anything else is a real name collision and the rescue
    /// lands beside it as the first free `<stem>.rescued-<n>[.<ext>]`.
    Sibling,
    /// A transcript only ever grows by appends. A source that is a byte-prefix
    /// of the target is a stale copy of it and is dropped; a target that is a
    /// byte-prefix of the source is the same conversation missing its tail, so
    /// that tail is appended to the target itself and `--resume <id>` opens the
    /// whole of it. Copies that each hold lines the other lacks collide as under
    /// [`Occupied::Sibling`].
    Append,
    /// The target is authoritative and the source a stale copy, dropped whatever
    /// its bytes or stamp: a copy host's shared tree took a copy of every global
    /// entry at its build, so a name the global store holds is one the tree
    /// inherited. An in-place edit its session made after its last mirror tick
    /// is lost with it.
    Stale,
}

/// The tree a sidecar rescue lifts from, which decides what an occupied global
/// name means and whether a linked global dir is written through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarSource {
    /// An isolated tree: it shares nothing with the global store, so an
    /// occupied name is a collision ([`Occupied::Sibling`]) and its session
    /// never wrote through a global link, which the rescue therefore skips.
    Isolated,
    /// A shared tree that is not positively a copy of the global store: an
    /// occupied name is that session's own ([`Occupied::Sibling`]). Its session
    /// wrote through the global store's links, so the rescue does too.
    Shared,
    /// A shared tree built as a copy of the global store, so an occupied name
    /// is a copy it inherited ([`Occupied::Stale`]); written through like
    /// [`SidecarSource::Shared`].
    SharedCopy,
}

impl SidecarSource {
    fn occupied(self) -> Occupied {
        match self {
            Self::Isolated | Self::Shared => Occupied::Sibling,
            Self::SharedCopy => Occupied::Stale,
        }
    }

    /// Whether the global entry `dst` is a link the rescue must not write
    /// through. Only a link resolving to a directory is ever followed, and only
    /// from a shared tree: a dangling link reads as a free name, so the move
    /// would replace it rather than write through it.
    fn refuses(self, dst: &Path) -> bool {
        is_symlink(dst) && (self == Self::Isolated || !dst.is_dir())
    }

    /// What the entry `src` the rescue [`refuses`](Self::refuses) at `dst`
    /// counts as, `depth` the cap its walk would have run under. An isolated
    /// session never wrote through the link, so its entry goes with the tree.
    /// On a copy, a file whose global counterpart exists, link or not, is a
    /// stale copy and is dropped as [`Occupied::Stale`] drops one. Any other
    /// shared entry is the session's own, met by a global link that dangles or
    /// names no directory: it counts the files the walk would have carried as
    /// left, so the tree is kept for a later sweep only when there is something
    /// in it to lose.
    fn refused(self, src: &Path, dst: &Path, depth: usize) -> Rescued {
        if self == Self::Isolated {
            return Rescued::default();
        }
        match src.symlink_metadata() {
            Ok(meta) if meta.is_dir() => Rescued {
                moved: 0,
                left: files_to_rescue(src, depth),
            },
            Ok(_) if self == Self::SharedCopy && dst.exists() => {
                let mut rescued = Rescued::default();
                rescued.record(src, std::fs::remove_file(src).map(|()| dst.to_path_buf()));
                rescued
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Rescued::default(),
            Ok(_) | Err(_) => Rescued { moved: 0, left: 1 },
        }
    }
}

/// How many files a sidecar walk of `dir` under `depth` would carry: every
/// non-link file down to the cap [`rescue_tree`] stops at. A dir or entry that
/// cannot be read counts one, since nothing shows it holds nothing.
fn files_to_rescue(dir: &Path, depth: usize) -> usize {
    if depth == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 1;
    };
    entries
        .map(
            |entry| match entry.and_then(|e| Ok((e.file_type()?, e.path()))) {
                Ok((file_type, _)) if file_type.is_symlink() => 0,
                Ok((file_type, path)) if file_type.is_dir() => files_to_rescue(&path, depth - 1),
                Ok(_) | Err(_) => 1,
            },
        )
        .sum()
}

/// Which copy an occupied rescue keeps.
enum Keep {
    /// The global one; the source is dropped.
    Target,
    /// The global one, extended in place by the source's bytes past this
    /// offset, which the global one already holds.
    Tail(u64),
    /// Both: the source lands beside the global one.
    Both,
}

/// How one rescue leg ended: the files it carried into the global store (an
/// already-present copy it dropped counts, matching the transcript leg), and
/// the files it was meant to carry and left in the source tree (an entry
/// [`SidecarSource::refused`] leaves counts the files under it). A source
/// that vanished under the rescue is in neither count: nothing of it is left
/// to lose.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rescued {
    pub(crate) moved: usize,
    pub(crate) left: usize,
}

impl Rescued {
    /// Count one file's rescue outcome. A failure whose source cannot be shown
    /// gone counts as left; only a `NotFound` stat proves it is, and a link
    /// that resolves to nothing holds nothing either.
    fn record(&mut self, src: &Path, outcome: std::io::Result<PathBuf>) {
        match outcome {
            Ok(_) => self.moved += 1,
            Err(_)
                if src
                    .metadata()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(e) => {
                logline!("clauth: failed to rescue {}: {e}", src.display());
                self.left += 1;
            }
        }
    }

    fn add(&mut self, other: Self) {
        self.moved += other.moved;
        self.left += other.left;
    }
}

/// Rescue a session transcript at `src` in a runtime tree's store into the
/// global store, preserving its `<slug>` subdir so `--resume` run from that
/// workspace still finds it. An occupied `<id>.jsonl` takes
/// [`Occupied::Append`]'s rule, so a target holding lines the source lacks is
/// never overwritten. Returns the final path.
pub(crate) fn rescue_session_transcript(
    src: &Path,
    tree_projects_root: &Path,
    global_projects_root: &Path,
) -> std::io::Result<PathBuf> {
    // A tree's store mirrors the global `<slug>/<id>.jsonl` layout, so the
    // existing subdir is authoritative — preserve it verbatim rather than
    // recomputing the (lossy) slug from cwd.
    let rel = src.strip_prefix(tree_projects_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not under the runtime projects root {}",
                src.display(),
                tree_projects_root.display()
            ),
        )
    })?;
    let target = global_projects_root.join(rel);
    rescue_file(src, &target, Occupied::Append)
}

/// Move `src` onto `target`; an occupied `target` takes `occupied`'s rule.
/// Returns the final path. A moved file goes through [`rescue_move`]'s
/// verified copy and an extended one through [`append_tail`]'s verified
/// append, so a crash leaves at worst both copies. The whole decision and its
/// landing run under [`RescueFileLock`], so a second rescue onto the same name
/// compares against the target as the first one left it.
fn rescue_file(src: &Path, target: &Path, occupied: Occupied) -> std::io::Result<PathBuf> {
    let _lock = RescueFileLock::acquire()?;
    if !target.exists() {
        rescue_move(src, target)?;
        return Ok(target.to_path_buf());
    }
    let keep = match occupied {
        Occupied::Stale => Keep::Target,
        // A directory sitting where the rescue wants a file is a name collision
        // like any other, and reading it would only error.
        Occupied::Sibling | Occupied::Append if target.is_dir() => Keep::Both,
        Occupied::Sibling if files_equal(src, target)? => Keep::Target,
        Occupied::Sibling => Keep::Both,
        Occupied::Append => {
            let source_len = std::fs::metadata(src)?.len();
            let held_len = std::fs::metadata(target)?.len();
            if source_len <= held_len {
                if same_prefix(src, target, source_len)? {
                    Keep::Target
                } else {
                    Keep::Both
                }
            } else if same_prefix(src, target, held_len)? {
                Keep::Tail(held_len)
            } else {
                Keep::Both
            }
        }
    };
    match keep {
        Keep::Target => {
            std::fs::remove_file(src)?;
            Ok(target.to_path_buf())
        }
        Keep::Tail(held) => {
            append_tail(src, target, held)?;
            Ok(target.to_path_buf())
        }
        Keep::Both => {
            let sibling = free_rescued_sibling(target)?;
            rescue_move(src, &sibling)?;
            Ok(sibling)
        }
    }
}

/// Rescue every `*.jsonl` under a runtime tree's `tree_projects` into the global
/// `global_projects` store. Fail-soft per file: a rescue error is logged and
/// counted as left, so one bad transcript never blocks the rest and a caller
/// can keep the tree that still holds it. Each move is collision- and crash-safe
/// (see [`rescue_session_transcript`]).
///
/// A symlinked `tree_projects` is never walked: it resolves into a store the
/// rescue would then "move" onto itself, deleting each file there as an
/// already-rescued copy of itself. That is every linking host's shared tree,
/// so the skip is silent.
pub(crate) fn rescue_transcripts(tree_projects: &Path, global_projects: &Path) -> Rescued {
    let mut rescued = Rescued::default();
    if is_symlink(tree_projects) {
        return rescued;
    }
    let mut paths = Vec::new();
    collect_jsonl(tree_projects, WALK_MAX_DEPTH, &mut paths);
    for src in paths {
        let outcome = rescue_session_transcript(&src, tree_projects, global_projects);
        rescued.record(&src, outcome);
    }
    rescued
}

/// The per-session sidecar trees a rescue lifts out of a runtime tree.
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
pub(crate) const SIDECAR_TREES: &[&str] = &[
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

/// Whether a top-level runtime-tree entry is session sidecar state to
/// rescue. Name-only: with an allowlist the entry's shape carries no safety
/// weight (the old "directories only" rule existed to bound what a denylist let
/// through), so the caller merges a dir and moves a file under the same name.
fn rescuable_sidecar(name: &std::ffi::OsStr) -> bool {
    SIDECAR_TREES.iter().any(|tree| name == *tree)
}

/// Rescue Claude Code's session sidecar state out of a runtime root into the
/// global `~/.claude/`. [`SIDECAR_TREES`] is the admission rule; every other
/// entry is left for the GC, named once in a log line for an isolated tree, so
/// a renamed tree is visible. An entry the global store already holds takes
/// `source`'s rule
/// ([`SidecarSource`]).
///
/// The merge is per ENTRY, never per tree: the global store usually already has
/// a `shell-snapshots/`, so moving the dir wholesale would replace it.
/// Fail-soft per entry like the transcript leg: a failure is logged and counted
/// as left.
///
/// A source link is skipped, never followed: what a tree links resolves into
/// the global store or past it, so walking one could reach the operator's own
/// store and move it onto itself. `~/.claude` can hold operator links pointing
/// outside the store (a `plans` dir symlinked into dotfiles): a shared tree's
/// rescue writes through a directory link the way its session did, an
/// isolated tree's never does ([`SidecarSource::refuses`]).
pub(crate) fn rescue_sidecars(
    tree_root: &Path,
    global_root: &Path,
    source: SidecarSource,
) -> Rescued {
    let mut rescued = Rescued::default();
    let Ok(entries) = std::fs::read_dir(tree_root) else {
        return rescued;
    };
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
        if source.refuses(&dst) {
            logline!(
                "clauth: skipping rescue of {}, {} is a symlink",
                src.display(),
                dst.display()
            );
            rescued.add(source.refused(&src, &dst, SIDECAR_MAX_DEPTH - 1));
            continue;
        }
        if file_type.is_dir() {
            rescued.add(rescue_tree(&src, &dst, SIDECAR_MAX_DEPTH - 1, source));
        } else {
            let outcome = rescue_file(&src, &dst, source.occupied());
            rescued.record(&src, outcome);
        }
    }
    // Only an isolated tree is all session-authored, so only there does an
    // unlisted dir hint at a renamed sidecar tree; a shared tree holds the
    // operator's own entries too.
    if source == SidecarSource::Isolated && !untouched.is_empty() {
        untouched.sort();
        logline!(
            "clauth: left {} in {} (not session state)",
            untouched.join(", "),
            tree_root.display()
        );
    }
    rescued
}

/// Merge one sidecar tree into its global counterpart entry by entry.
/// Source links are skipped rather than followed (so no link cycle is entered),
/// and a global link only as `source` allows. Reaching the depth cap is logged,
/// never silent: what it drops is state in a tree about to be discarded.
fn rescue_tree(src: &Path, dst: &Path, depth: usize, source: SidecarSource) -> Rescued {
    let mut rescued = Rescued::default();
    if depth == 0 {
        logline!(
            "clauth: rescue depth cap reached at {}, leaving the subtree",
            src.display()
        );
        return rescued;
    }
    let Ok(entries) = std::fs::read_dir(src) else {
        return rescued;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if source.refuses(&target) {
            logline!(
                "clauth: skipping rescue of {}, {} is a symlink",
                path.display(),
                target.display()
            );
            rescued.add(source.refused(&path, &target, depth - 1));
            continue;
        }
        if file_type.is_dir() {
            rescued.add(rescue_tree(&path, &target, depth - 1, source));
            continue;
        }
        let outcome = rescue_file(&path, &target, source.occupied());
        rescued.record(&path, outcome);
    }
    rescued
}

/// Whether `path` is a symlink itself (never following it); a Windows junction
/// reads as one too. A missing path is not one.
pub(crate) fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether two files hold identical bytes. Length is checked first as a cheap
/// reject before comparing them.
fn files_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    let len = std::fs::metadata(a)?.len();
    if len != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    same_prefix(a, b, len)
}

/// Whether the first `len` bytes of `a` and `b` are equal, compared a bounded
/// chunk at a time so two large transcripts never sit in memory whole. Each
/// file must hold at least `len` bytes; a shorter one is an error.
fn same_prefix(a: &Path, b: &Path, len: u64) -> std::io::Result<bool> {
    const CHUNK: usize = 64 * 1024;
    let mut a = File::open(a)?;
    let mut b = File::open(b)?;
    let mut chunk_a = vec![0u8; CHUNK];
    let mut chunk_b = vec![0u8; CHUNK];
    let mut left = len;
    while left > 0 {
        let n = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        a.read_exact(&mut chunk_a[..n])?;
        b.read_exact(&mut chunk_b[..n])?;
        if chunk_a[..n] != chunk_b[..n] {
            return Ok(false);
        }
        left -= n as u64;
    }
    Ok(true)
}

/// Append `src`'s bytes past `held` to `target`, which already holds `src`'s
/// first `held` bytes, then drop `src` once `target` is shown to start with
/// the whole of it. The write lands in the target file itself (`O_APPEND`), so
/// a process holding it open keeps writing into the file the store names. A
/// crash mid-append leaves `target` a longer prefix of `src`, which the next
/// rescue extends; a line some other writer appended meanwhile fails the
/// re-check and keeps `src`.
fn append_tail(src: &Path, target: &Path, held: u64) -> std::io::Result<()> {
    let mut source = File::open(src)?;
    let source_len = source.metadata()?.len();
    source.seek(SeekFrom::Start(held))?;
    let mut global = std::fs::OpenOptions::new().append(true).open(target)?;
    std::io::copy(&mut source, &mut global)?;
    global.sync_all()?;
    if global.metadata()?.len() < source_len || !same_prefix(src, target, source_len)? {
        return Err(std::io::Error::other(format!(
            "rescue verify failed: {} does not start with source {}",
            target.display(),
            src.display()
        )));
    }
    std::fs::remove_file(src)
}

/// The lock file [`RescueFileLock`] flocks, under `~/.clauth`. Never removed,
/// like every static flock file: unlinking it under a hold would split racers
/// across two inodes.
const RESCUE_FILE_LOCK_FILENAME: &str = "rescue.lock";

/// How long one rescue scope waits for [`RescueFileLock`], in aggregate over
/// every file it rescues ([`RescueLockBudget`]): a tree's rescue, or a whole
/// sweep or profile delete over every tree it walks. A hold is one file's
/// compare, copy or append and its verify: local IO, no subprocess or network.
/// A file whose wait runs out is left in its tree, which is kept for a later
/// sweep.
const RESCUE_FILE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// The box-wide lock one global name's rescue holds from its first look at
/// the target through the verify of what it landed. Two rescues of different
/// trees can carry diverging tails of one transcript: under the lock, the
/// second compares against the global file as the first left it, sees the
/// divergence and lands beside it instead of appending a second tail. Held per
/// file, never across a walk. Ranked OUTSIDE the state flock
/// ([`crate::lockorder::rank::RescueFile`]), so no rescue waits on it with the
/// flock held.
struct RescueFileLock {
    /// Closing the fd releases the flock; drops before `_rank`.
    _held: File,
    _rank: crate::lockorder::RankGuard,
}

impl RescueFileLock {
    /// Take the flock within what [`RescueLockBudget`] leaves, or
    /// [`RESCUE_FILE_LOCK_WAIT`] outside one; a wait that runs out is an
    /// error, so the caller leaves the file where it is.
    fn acquire() -> std::io::Result<Self> {
        let rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::RescueFile>();
        let dir = clauth_dir().map_err(std::io::Error::other)?;
        mkdir_700(&dir)?;
        let path = dir.join(RESCUE_FILE_LOCK_FILENAME);
        let file = crate::profile::open_state_file(&path)?;
        crate::lock::lock_file_with_timeout(
            &file,
            RescueLockBudget::remaining(),
            |_| {
                anyhow::anyhow!(
                    "another rescue held {} past this rescue's wait; the file is left \
                     for a later sweep",
                    path.display()
                )
            },
            "failed to lock the rescue lock",
        )
        .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        Ok(Self {
            _held: file,
            _rank: rank,
        })
    }
}

thread_local! {
    /// The deadline every [`RescueFileLock`] wait under the current
    /// [`RescueLockBudget`] shares; `None` outside one.
    static RESCUE_LOCK_DEADLINE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// One [`RESCUE_FILE_LOCK_WAIT`] shared by every [`RescueFileLock`] wait of a
/// scope (a tree's rescue, a whole sweep, a profile delete), so a wedged holder
/// costs that scope the bound once, never once per file. Arm-if-not-armed: an
/// inner scope spends the outer one's deadline, and only the arming scope
/// disarms it.
pub(crate) struct RescueLockBudget {
    armed: bool,
}

impl RescueLockBudget {
    pub(crate) fn arm() -> Self {
        let armed = RESCUE_LOCK_DEADLINE.with(|deadline| {
            if deadline.get().is_some() {
                return false;
            }
            deadline.set(Some(std::time::Instant::now() + rescue_file_lock_wait()));
            true
        });
        Self { armed }
    }

    /// What is left of the armed scope's wait, or a whole
    /// [`RESCUE_FILE_LOCK_WAIT`] outside one.
    fn remaining() -> Duration {
        RESCUE_LOCK_DEADLINE
            .with(std::cell::Cell::get)
            .map_or_else(rescue_file_lock_wait, |deadline| {
                deadline.saturating_duration_since(std::time::Instant::now())
            })
    }
}

impl Drop for RescueLockBudget {
    fn drop(&mut self) {
        if self.armed {
            RESCUE_LOCK_DEADLINE.with(|deadline| deadline.set(None));
        }
    }
}

/// [`RESCUE_FILE_LOCK_WAIT`], or a shorter wait a test poses on its own thread.
fn rescue_file_lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = RESCUE_LOCK_WAIT_OVERRIDE.with(std::cell::Cell::get) {
        return wait;
    }
    RESCUE_FILE_LOCK_WAIT
}

#[cfg(test)]
thread_local! {
    static RESCUE_LOCK_WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Set or clear the current thread's [`rescue_file_lock_wait`] override.
#[cfg(test)]
pub(crate) fn set_rescue_lock_wait_override(wait: Option<Duration>) {
    RESCUE_LOCK_WAIT_OVERRIDE.with(|cell| cell.set(wait));
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
            Some(ext) => format!("{stem}{RESCUED_TAG}{n}.{ext}"),
            None => format!("{stem}{RESCUED_TAG}{n}"),
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

/// The infix [`free_rescued_sibling`] puts between a rescued file's stem and
/// its number.
const RESCUED_TAG: &str = ".rescued-";

/// The file name a rescue renamed into `name` (`<stem>.rescued-<n>[.<ext>]`
/// back to `<stem>[.<ext>]`), `None` when `name` is not a rescued sibling.
pub(crate) fn rescued_source_name(name: &str) -> Option<String> {
    let (stem, rest) = name.rsplit_once(RESCUED_TAG)?;
    let (n, ext) = match rest.split_once('.') {
        Some((n, ext)) => (n, Some(ext)),
        None => (rest, None),
    };
    if stem.is_empty()
        || n.is_empty()
        || !n.bytes().all(|b| b.is_ascii_digit())
        || ext.is_some_and(|e| e.contains('.'))
    {
        return None;
    }
    Some(match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    })
}

#[cfg(test)]
#[path = "../tests/inline/sessions.rs"]
mod tests;
