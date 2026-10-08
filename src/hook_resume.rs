//! The herdr resume-command report leg of `clauth
//! hook-profile-changed-note`: tell the herdr pane a conversation runs in how
//! to bring that conversation back, so a herdr restart restores it as
//! `clauth resume <session> --profile <account>` instead of herdr's builtin
//! bare `claude --resume <id>` — which loses the account and answers `No
//! conversation found` for isolated-store sessions.
//!
//! The hook is the only component that natively knows both halves of that
//! command: the CC session id arrives in the hook payload, and the resolved
//! account is the same tier-walk answer the account note and the exact-owner
//! stamp carry. As a Claude Code child inside the pane it also inherits
//! `HERDR_PANE_ID`/`HERDR_BIN_PATH`, which the herdr plugin's own watcher
//! cannot see (it joins on pids and only ever learns clauth's minted ids).
//!
//! Fires on `SessionStart` and on a real attribution change — the two events
//! that can change the resume command (the session id or the profile moved) —
//! and, while herdr has refused the last report, on every main-scope fire.
//! Outside a herdr pane (no `HERDR_PANE_ID`, or no herdr binary), without an
//! attributable account, and on any failure of the bounded report spawn, the
//! leg is silence: herdr's builtin plan stays as the fallback, which is
//! exactly the restore behavior a box had before this leg existed.
//!
//! The report is `pane report-agent-session` with no state and no session id.
//! herdr records a resume argv from any source while no reporter holds the
//! pane's state and the named agent is the one it detects there, and a session
//! report without a session id moves neither the pane's state nor its session
//! owner, so the chip stays herdr's own screen detection. It must never be
//! `pane report-agent`: herdr 0.9 keeps a state reported from a non-herdr source
//! as the pane's lifecycle authority, above its own detection, until the agent
//! exits, and refuses every later state report from that source once its own
//! Claude Code integration has reported the session — a `--state working`
//! report that landed first pinned the pane at `working` for the rest of its
//! life, with only a visible blocker showing through.
//!
//! A `SessionStart` can reach herdr before its detector has seen Claude in the
//! pane; herdr then answers `resume_not_accepted` and records nothing. The
//! hook never waits for detection inside a fire, which would hold the
//! conversation's start: the refusal arms a re-send on the conversation's
//! note record, and the next main-scope fire (a prompt or a tool call) sends
//! the report again with a fresh seq, until one is accepted. herdr answers the
//! same code for causes that never clear (another source holding the pane's
//! state, a non-claude agent detected there), so on such a pane every prompt
//! and tool call re-sends: one bounded spawn each, logged once when armed.
//! Only that code arms it; any other failure disarms it, so a broken or hung
//! herdr is never re-spawned per tool call, and one failed re-send leaves the
//! report to the next attribution change.
//!
//! The seq is minted epoch-ms and persisted on the conversation's note
//! record, because herdr drops a report whose seq is not strictly newer than
//! the stored one — a fresh process whose first mint landed on the same
//! millisecond as the previous process's last report would otherwise be
//! silently lost.

use std::path::Path;

use crate::hook_note::{Payload, ScopeLock, load_record, record_path, store_record};

/// The report's fixed agent fields: clauth speaks as source `clauth` about
/// the `claude` agent herdr detects in the pane. `herdr:claude` itself is
/// reserved for herdr's own CC integration; a distinct source is what lets
/// the report ride beside it instead of replacing it.
const SOURCE: &str = "clauth";
const AGENT: &str = "claude";

/// herdr's error code for a resume argv it cannot attach to the pane yet.
const NOT_ACCEPTED: &str = "resume_not_accepted";

/// How long the record's mint and re-send writes wait on the scope lock; see
/// [`ScopeLock::acquire_within`].
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// What one report came back with, as far as the re-send cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Accepted,
    /// herdr refused the argv as not attachable yet and recorded nothing.
    NotAccepted,
    Failed,
}

pub(crate) fn report_for_fire(payload: &Payload, current: Option<&str>, attribution_changed: bool) {
    // A subagent's fire is its own scope's reading, never the pane
    // conversation's attribution.
    if payload.agent_id.is_some() {
        return;
    }
    // No attributable account: nothing to name, herdr's builtin plan stays.
    let Some(profile) = current else {
        return;
    };
    // Outside a herdr pane there is nothing to report to.
    let Some(pane_id) = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
    else {
        return;
    };
    let Ok(path) = record_path(&payload.session_id, None) else {
        return;
    };
    // Only a `SessionStart` or a real attribution change can move the resume
    // command — its session id or its profile changed — and only an armed
    // re-send still owes one, so every other fire (per tool call, per prompt)
    // is a no-op.
    let armed = load_record(&path).is_some_and(|record| record.resume_pending);
    if payload.event != "SessionStart" && !attribution_changed && !armed {
        return;
    }
    let Some(bin) = crate::herdr::resolved_bin() else {
        return;
    };
    let Some(seq) = mint_seq(&path) else {
        return;
    };
    let args = build_report_args(&pane_id, &payload.session_id, profile, seq);
    let refused = report(&bin, &args) == Sent::NotAccepted;
    if refused != armed {
        if refused {
            crate::logline::to_logfile(format_args!(
                "clauth: herdr has not accepted the resume command yet; re-sending on each fire until it does"
            ));
        }
        arm_resend(&path, refused);
    }
}

/// Mint the report seq for this conversation's main scope: epoch-ms, forced
/// past every seq this record has minted. herdr silently keeps the stored
/// argv for any report whose seq is not strictly newer than the last, so the
/// mint must be monotone across the separate hook processes one conversation
/// fires — the record is what turns the wall clock into a per-session clock.
/// `None` when the record cannot be persisted: an unrememberable seq cannot
/// promise monotonicity, and silence keeps herdr's existing argv standing.
fn mint_seq(path: &Path) -> Option<u64> {
    let _hold = ScopeLock::acquire_within(LOCK_WAIT);
    let mut record = load_record(path).unwrap_or_default();
    let seq = crate::usage::now_ms().max(record.resume_seq.unwrap_or(0).saturating_add(1));
    record.resume_seq = Some(seq);
    store_record(path, &record).ok()?;
    Some(seq)
}

/// Arm or disarm the next fire's re-send. A failed write is a logged silence:
/// an arm lost costs the report until the next attribution change, a disarm
/// lost costs one redundant accepted report.
fn arm_resend(path: &Path, armed: bool) {
    let _hold = ScopeLock::acquire_within(LOCK_WAIT);
    let mut record = load_record(path).unwrap_or_default();
    record.resume_pending = armed;
    if let Err(e) = store_record(path, &record) {
        crate::logline::to_logfile(format_args!(
            "clauth: resume report re-send state not stored: {e}"
        ));
    }
}

/// The report argv, pane id first — herdr's hand-rolled parser reads the
/// first positional from args[0] and answers `unknown option` to anything
/// else in that slot. Everything after `--` is the resume command herdr
/// re-creates the pane with.
fn build_report_args(pane_id: &str, session_id: &str, profile: &str, seq: u64) -> Vec<String> {
    vec![
        "pane".into(),
        "report-agent-session".into(),
        pane_id.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--seq".into(),
        seq.to_string(),
        "--".into(),
        "clauth".into(),
        "resume".into(),
        session_id.into(),
        "--profile".into(),
        profile.into(),
    ]
}

/// One bounded spawn of the herdr CLI; every failure but herdr's
/// not-yet-accepted refusal is a logged silence, the same posture the account
/// note keeps. The caller logs that refusal once, when it arms the re-send.
/// [`crate::herdr::bounded_output`]
/// kills a child that hangs past its deadline, so a stuck herdr costs the
/// hook at most that bound.
fn report(bin: &Path, args: &[String]) -> Sent {
    let Some(bin) = bin.to_str() else {
        crate::logline::to_logfile(format_args!(
            "clauth: herdr bin path is not valid UTF-8; resume command not reported"
        ));
        return Sent::Failed;
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::herdr::bounded_output(bin, &argv, &[]) {
        Some(output) if output.status.success() => Sent::Accepted,
        Some(output) => {
            let status = output.status;
            let out = crate::daemon::api::panes::HerdrOut::from(output);
            if crate::daemon::api::agent::error_code(&out).as_deref() == Some(NOT_ACCEPTED) {
                return Sent::NotAccepted;
            }
            crate::logline::to_logfile(format_args!(
                "clauth: herdr pane report-agent-session exited {status} (resume command not reported)"
            ));
            Sent::Failed
        }
        None => {
            crate::logline::to_logfile(format_args!(
                "clauth: herdr pane report-agent-session failed to spawn or timed out (resume command not reported)"
            ));
            Sent::Failed
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/hook_resume.rs"]
mod tests;
