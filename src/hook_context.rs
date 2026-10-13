//! The context-window nudge leg of `clauth hook-profile-changed-note`: one
//! note per conversation once its CC transcript's context usage crosses the
//! configured threshold, riding the envelope the account note and the
//! headroom nudge share.
//!
//! The threshold lives in `AppState.context_nudge_threshold_tokens` (`None` =
//! off). Usage is the last usage-bearing JSONL line of the conversation
//! transcript — the payload's `transcript_path`, falling back to the record's
//! stored one — read from the TAIL of the file only (the last 1 MiB:
//! transcripts reach hundreds of MB). The tail is a bounded window and
//! everything outside it is invisible by design: a `Task` whose `tool_use`
//! fell out of the window is not counted as a running agent, and a usage
//! line straddling the window is lost. Streaming partials of one assistant
//! turn repeat its content across lines, so open `Task` ids are counted once
//! per id — one open task is one agent however many partials carry it.
//!
//! The copy varies by auto-compact state (`autoCompactEnabled` from
//! `$CLAUDE_CONFIG_DIR/settings.json`, fallback `~/.claude/settings.json`,
//! default true — the client's own default) and by what is still running:
//! open `Task` tool_use entries in the tail (a `tool_use` whose id has no
//! matching `tool_result` anywhere in the tail) and the live delegate jobs
//! this conversation's `claude` process spawned (a peer session's are not
//! its to wait on). The running clause rides the auto-compact OFF arm only.
//!
//! One note per conversation per threshold: the main-scope record's
//! `context.told_threshold` — read-modify-write under `hook_note`'s
//! `ScopeLock`, the same lock the other two legs take — suppresses a second
//! emit, a `SessionStart` with `source: "compact"` re-announces while the
//! usage is still above (the compaction dropped the injected note from
//! context), and a threshold turned off clears the stamp when an off-state
//! fire lands; turned off and back on between two fires, the record never
//! sees the off state, so the same threshold stays suppressed until a
//! compaction re-announces it. Main scope
//! only: a fire carrying `agent_id` is inert, the record write included —
//! one scope never touches another scope's bytes.
//!
//! A failure is silence at exit 0, never an error: a hook that errors on a
//! tool call breaks the conversation it exists to inform.

use crate::format::format_threshold_tokens;
use crate::hook_note::{Payload, ScopeLock, load_record, record_path, store_record};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How much of a transcript's tail one fire reads: the last 1 MiB, never the
/// whole file. The window is best-effort by design (see the module doc).
const TAIL_BYTES: u64 = 1024 * 1024;

/// The leg's memory for one scope: which threshold it last told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextState {
    #[serde(default)]
    pub(crate) told_threshold: Option<u64>,
}

/// Everything the leg read to decide a fire.
#[derive(Debug)]
struct ContextRead {
    /// `None` = the feature is off (or a hand-edit read as off).
    threshold: Option<u64>,
    /// Context tokens off the transcript's last usage-bearing line. `None` =
    /// no transcript could be read.
    usage: Option<u64>,
    /// Whether the conversation auto-compacts; read only when the fire may
    /// emit, and only the copy uses it.
    auto_compact: bool,
    /// Open `Task` tool_use entries in the tail; copy-only, same gating.
    agents: usize,
    /// Live delegate jobs this conversation's process spawned; copy-only,
    /// same gating.
    delegates: usize,
}

/// What one bounded tail read found.
#[derive(Debug)]
struct TailView {
    usage: Option<u64>,
    open_tasks: usize,
}

/// The leg's entry: read what this fire needs, then decide and store. Every
/// failure — an unreadable config, a missing transcript, a store error — is
/// silence here, by `hook_note`'s module-wide contract.
pub(crate) fn note(payload: &Payload) -> Option<String> {
    let read = read_context(payload)?;
    context_note(payload, &read)
}

fn read_context(payload: &Payload) -> Option<ContextRead> {
    // Main scope only: a subagent fire never earns the note, and never
    // touches the main record either — one scope never touches another
    // scope's bytes (`hook_note`'s module doc).
    if payload.agent_id.is_some() {
        return None;
    }
    let threshold = crate::profile::load_app_state()
        .ok()?
        .context_nudge_threshold_tokens();
    let Some(th) = threshold else {
        // Off: still a read, so the note side can clear a stale told stamp.
        return Some(ContextRead {
            threshold: None,
            usage: None,
            auto_compact: false,
            agents: 0,
            delegates: 0,
        });
    };
    // The payload's transcript_path first, the record's stored one as the
    // fallback — a fire that carries no path (a SessionStart can land before
    // Claude Code creates the transcript) reads what an earlier fire stored.
    let path = payload.transcript.clone().or_else(|| {
        let stored = record_path(&payload.session_id, None).ok()?;
        load_record(&stored).and_then(|r| r.transcript)
    });
    let tail = path.as_deref().and_then(|p| read_tail(p, TAIL_BYTES));
    let usage = tail.as_ref().and_then(|t| t.usage);
    let mut read = ContextRead {
        threshold: Some(th),
        usage,
        auto_compact: false,
        agents: 0,
        delegates: 0,
    };
    // The three copy inputs only matter when the fire may emit.
    if read.usage.is_some_and(|u| u >= th) {
        read.auto_compact = auto_compact_enabled();
        read.agents = tail.as_ref().map_or(0, |t| t.open_tasks);
        read.delegates = live_delegates(&payload.session_id);
    }
    Some(read)
}

/// Decide and store, under the same scope lock the account and nudge legs
/// take: the emit is one read-modify-write per fire, and a durable write
/// only happens when a decision changed the record — the emit and the
/// clear-when-off are the only two.
fn context_note(payload: &Payload, read: &ContextRead) -> Option<String> {
    let path = record_path(&payload.session_id, None).ok()?;
    let _hold = ScopeLock::acquire();
    let stored = load_record(&path);
    let mut record = stored.clone().unwrap_or_default();
    if payload.transcript.is_some() {
        record.transcript = payload.transcript.clone();
    }
    let Some(th) = read.threshold else {
        // Off: clear the told stamp so a threshold turned off and back on
        // re-arms. The one write this branch ever makes.
        if record
            .context
            .as_ref()
            .is_some_and(|c| c.told_threshold.is_some())
        {
            record.context = None;
            if stored.as_ref() != Some(&record) && store_record(&path, &record).is_err() {
                crate::logline::to_logfile(format_args!(
                    "hook-note: cannot persist {}; staying silent",
                    path.display()
                ));
            }
        }
        return None;
    };
    let usage = read.usage?;
    if usage < th {
        return None;
    }
    let told = record.context.as_ref().and_then(|c| c.told_threshold);
    // A compaction dropped the injected note from context, so a SessionStart
    // `compact` with the usage still above re-announces.
    let compact_reemit = payload.event == "SessionStart"
        && payload.source.as_deref() == Some("compact")
        && told == Some(th);
    if told == Some(th) && !compact_reemit {
        return None;
    }
    let note = render_context_note(th, read.auto_compact, read.agents, read.delegates);
    record.context = Some(ContextState {
        told_threshold: Some(th),
    });
    if stored.as_ref() != Some(&record) && store_record(&path, &record).is_err() {
        crate::logline::to_logfile(format_args!(
            "hook-note: cannot persist {}; staying silent",
            path.display()
        ));
        return None;
    }
    Some(note)
}

/// The note copy, byte-pinned by the tests. Sentences after the first start
/// lowercase and are joined by single spaces; the running clause rides the
/// auto-compact OFF arm only and drops zero counts.
fn render_context_note(
    threshold: u64,
    auto_compact: bool,
    agents: usize,
    delegates: usize,
) -> String {
    let threshold = format_threshold_tokens(threshold);
    let mut note = format!("clauth: context window usage has exceeded {threshold} tokens.");
    if auto_compact {
        note.push_str(" auto-compaction is turned on.");
    } else {
        note.push_str(
            " consider writing or updating a handoff prompt so a fresh session can take over.",
        );
        let mut running: Vec<String> = Vec::new();
        if agents > 0 {
            running.push(format!(
                "{agents} {}",
                if agents == 1 { "agent" } else { "agents" }
            ));
        }
        if delegates > 0 {
            running.push(format!(
                "{delegates} {}",
                if delegates == 1 {
                    "delegate"
                } else {
                    "delegates"
                }
            ));
        }
        if !running.is_empty() {
            note.push_str(&format!(
                " there are still {} running. wait for them to return before closing the session.",
                running.join(" and ")
            ));
        }
    }
    note
}

/// The last `budget` bytes of `path`, parsed as JSONL.
///
/// The window usually opens mid-line, so the first line is a partial and the
/// parse skips it. Streaming partials of one assistant turn repeat its
/// content across lines, so open `Task` ids are deduplicated per id: one
/// open task is one agent however many partials carry it.
fn read_tail(path: &Path, budget: u64) -> Option<TailView> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(budget)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let mut usage = None;
    let mut open: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(parsed) = serde_json::from_slice::<TailLine>(line) else {
            continue;
        };
        let Some(msg) = parsed.message else {
            continue;
        };
        if let Some(u) = msg.usage {
            usage = Some(context_tokens(&u));
        }
        if let Some(blocks) = msg.content {
            for block in blocks {
                match block.kind.as_deref() {
                    Some("tool_result") => {
                        if let Some(id) = block.tool_use_id {
                            results.push(id);
                        }
                    }
                    Some("tool_use") if block.name.as_deref() == Some("Task") => {
                        if let Some(id) = block.id
                            && !open.contains(&id)
                        {
                            open.push(id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let open_tasks = open.iter().filter(|id| !results.contains(id)).count();
    Some(TailView { usage, open_tasks })
}

/// Context tokens of one usage row. The shape decides: a whole-prompt row
/// (`input >= cache_read`) already counts the cached tokens inside `input`;
/// an anthropic row (`input < cache_read`) keeps them apart, so all three
/// fields sum.
fn context_tokens(u: &TailUsage) -> u64 {
    if u.input_tokens >= u.cache_read_input_tokens {
        u.input_tokens
    } else {
        u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens
    }
}

/// Whether the conversation auto-compacts: `autoCompactEnabled` from
/// `$CLAUDE_CONFIG_DIR/settings.json`, falling back to
/// `~/.claude/settings.json`, defaulting true when the key is absent — the
/// client's own default.
fn auto_compact_enabled() -> bool {
    let Some(path) = settings_json_path() else {
        return true;
    };
    read_auto_compact(&path)
}

fn settings_json_path() -> Option<PathBuf> {
    Some(crate::which::session_config_dir_or_global()?.join("settings.json"))
}

fn read_auto_compact(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SettingsFile>(&bytes).ok())
        .and_then(|settings| settings.auto_compact_enabled)
        .unwrap_or(true)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SettingsFile {
    #[serde(rename = "autoCompactEnabled")]
    auto_compact_enabled: Option<bool>,
}

/// Live delegate jobs (`Running` or `Blocking`) this conversation's process
/// spawned.
fn live_delegates(session_id: &str) -> usize {
    let own_pid = own_claude_pid();
    crate::mcp::jobs::list(crate::usage::now_ms())
        .into_iter()
        .filter(|job| spawned_here(&job.record, session_id, own_pid))
        .filter(|job| job.phase().is_live())
        .count()
}

/// Whether `record` was spawned by the process hosting `session_id`. The pid
/// decides wherever both sides know it, since `/clear` moves the session id
/// while the process and its server live on. Otherwise a record carrying
/// `host_session` counts only when it equals `session_id`: the conversation
/// the job was minted in where the host's session record answered, else the
/// server's startup id; one without it (an older server still running
/// across a self-update, a server launched outside Claude Code) counts: an
/// over-count only delays a close, an under-count orphans a paid run.
fn spawned_here(
    record: &crate::mcp::jobs::JobRecord,
    session_id: &str,
    own_pid: Option<u32>,
) -> bool {
    match own_pid {
        Some(pid) if record.host_pid != 0 => record.host_pid == pid,
        _ => record
            .host_session
            .as_deref()
            .is_none_or(|host| host == session_id),
    }
}

/// The `claude` process running this hook: Claude Code exports `CLAUDE_PID`
/// to its hook commands.
fn own_claude_pid() -> Option<u32> {
    std::env::var("CLAUDE_PID")
        .ok()?
        .parse()
        .ok()
        .filter(|pid| *pid != 0)
}

// ── JSONL transcript wire types ──────────────────────────────────────────────
//
// The same field names `tokens.rs` reads; this leg reads a bounded TAIL of
// the file and needs `name`/`id`/`tool_use_id` on content blocks, so it
// carries its own minimal shapes rather than reusing that sweep's.

#[derive(Deserialize, Default)]
#[serde(default)]
struct TailLine {
    message: Option<TailMessage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TailMessage {
    usage: Option<TailUsage>,
    content: Option<Vec<TailBlock>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TailUsage {
    input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TailBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    /// `tool_use` blocks: the id a later `tool_result` matches on.
    id: Option<String>,
    /// `tool_result` blocks: the `tool_use` id they close.
    tool_use_id: Option<String>,
}

#[cfg(test)]
#[path = "../tests/inline/hook_context.rs"]
mod tests;
