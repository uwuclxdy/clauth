//! Uncollected-results leg of `clauth hook-profile-changed-note`: on a
//! `SessionStart` resume or compaction, names the background delegates this
//! conversation started whose results still sit in the job store, riding the
//! envelope the other legs share.
//!
//! The `mcp-await-job` delivery hook is bound to the session that spawned the
//! job, so a result still uncollected when that session exited has no other
//! way back into the conversation: a resumed session starts a new process
//! whose own delivery hook never saw the job. A compaction drops whatever a
//! resume injected, so it names them again. The store is the ledger of what is
//! unread: both delivery paths evict a record through `jobs::claim`, so a
//! collected result is never named, and a record the startup sweep is about to
//! reap is never offered.
//!
//! Matched on `host_session` alone, strictly. `hook_context`'s `spawned_here`
//! counts a record with no host as a match, which suits its question (an
//! over-count only delays a close); here it would hand this conversation a
//! peer's result. The pid is no use either: a resume is a new process, so a
//! record's `host_pid` names the dead one. A fork gets a new session id no
//! record carries, so it is never told.

use crate::hook_note::Payload;
use crate::mcp::jobs::{self, JobPhase};

/// The most job ids one note names; the rest are counted. Bounds the injected
/// context on a fan-out's worth of results.
const NAMED_MAX: usize = 10;

pub(crate) fn note(payload: &Payload) -> Option<String> {
    note_at(payload, crate::usage::now_ms())
}

fn note_at(payload: &Payload, now: u64) -> Option<String> {
    if payload.event != "SessionStart"
        || !matches!(payload.source.as_deref(), Some("resume" | "compact"))
        || payload.agent_id.is_some()
    {
        return None;
    }
    let waiting: Vec<(String, String)> = jobs::list(now)
        .into_iter()
        .filter(|job| {
            job.phase() == JobPhase::Done
                && !jobs::done_is_expired(&job.record, now)
                && job.record.host_session.as_deref() == Some(payload.session_id.as_str())
        })
        .map(|job| (job.record.job_id, job.record.profile))
        .collect();
    render(&waiting)
}

/// The copy over `(job_id, profile)` pairs in the store's newest-first order.
fn render(waiting: &[(String, String)]) -> Option<String> {
    if waiting.is_empty() {
        return None;
    }
    let mut named = waiting
        .iter()
        .take(NAMED_MAX)
        .map(|(id, profile)| format!("`{id}` on `{profile}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let rest = waiting.len().saturating_sub(NAMED_MAX);
    if rest > 0 {
        named.push_str(&format!(", and {rest} more (`clauth jobs` lists them)"));
    }
    Some(if waiting.len() == 1 {
        format!(
            "clauth note: a background delegate this session started has a result waiting uncollected: {named}. `monitor` with its job_id collects it."
        )
    } else {
        format!(
            "clauth note: background delegates this session started have results waiting uncollected: {named}. `monitor` with their job_ids collects them."
        )
    })
}

#[cfg(test)]
#[path = "../tests/inline/hook_jobs.rs"]
mod tests;
