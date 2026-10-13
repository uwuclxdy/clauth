//! `GET /api/v1/panes` — every herdr pane on this host, joined on process ids
//! to the clauth sessions running inside them.
//!
//! The join is on pids only: a registry row belongs to a pane when its pid is
//! the pane's foreground process group or one of the processes running inside
//! it, and its kind is what that matching process is — a `clauth start` is the
//! pane's own session, anything else is a delegate. The `tokens.clauth` display
//! tag rides along and never joins anything.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::http::{Request, Response};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::herdr::HerdrPane;
use crate::live_sessions::LiveSession;

/// One bounded herdr call's outcome. herdr prints a success's JSON on stdout
/// and every error envelope (a refused request, a server it cannot reach) on
/// stderr with an empty stdout (measured on 0.9.0 with the streams separated),
/// so both streams ride.
pub(crate) struct HerdrOut {
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl From<std::process::Output> for HerdrOut {
    fn from(out: std::process::Output) -> Self {
        Self {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        }
    }
}

/// What the seam answered for one herdr call.
pub(crate) enum HerdrProbeOut {
    /// No binary resolved: herdr is not installed on this host.
    NotInstalled,
    /// A binary resolved and this is the bounded call's outcome; `None` when
    /// the call itself never ran (spawn failed, or it was killed on deadline).
    Ran(Option<HerdrOut>),
}

/// The seam between the daemon's routes and the herdr subprocess, argv and a
/// per-call deadline in and the bounded call's outcome out, so no test runs a
/// real herdr: this route's `pane list` and `process-info`, the terminal
/// bridge's `api snapshot`, the agent routes' `agent prompt` and `pane
/// send-keys`, and the session route's `tab create`/`agent start`/`pane run`
/// all go through it. The daemon fills [`ApiContext::herdr_probe`] with
/// [`real_probe`]; tests fill it with fixture-backed probes.
pub(crate) type PaneProbe =
    Box<dyn Fn(&[&str], std::time::Duration) -> HerdrProbeOut + Send + Sync>;

/// The real probe: [`crate::herdr::resolved_bin`] + the daemon-scoped
/// [`crate::herdr::daemon_bounded_output_deadline`] (session env stripped) at
/// the call's own deadline.
pub(crate) fn real_probe() -> PaneProbe {
    Box::new(|args, deadline| match crate::herdr::resolved_bin() {
        None => HerdrProbeOut::NotInstalled,
        Some(bin) => HerdrProbeOut::Ran(
            // The daemon-scoped call: the session env is stripped so a daemon
            // started inside a herdr pane still serves the default session
            // (owner ruling 2026-09-15, row 7; threat-model HB-4).
            crate::herdr::daemon_bounded_output_deadline(&bin.to_string_lossy(), args, deadline)
                .map(HerdrOut::from),
        ),
    })
}

/// A probe that always answers the absent shape: the default for tests that
/// build a context but never ask for panes.
#[cfg(test)]
pub(crate) fn absent_probe() -> PaneProbe {
    Box::new(|_, _| HerdrProbeOut::NotInstalled)
}

/// The one fixed sentence each absent state carries; nothing off the wire.
/// Shared with the terminal bridge, which answers the same two states.
pub(crate) const NOT_INSTALLED: &str = "herdr is not installed on this host";
pub(crate) const NO_SERVER: &str = "herdr is installed but no server answered on its socket";
/// The one fixed sentence a pane id herdr does not know carries, on every
/// route that names a pane.
pub(crate) const PANE_NOT_FOUND: &str = "no pane with that id in herdr's default session";

/// `herdr pane process-info`'s JSON envelope.
#[derive(Deserialize)]
struct ProcessInfoEnvelope {
    result: ProcessInfoResult,
}

#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

/// herdr omits BOTH keys for a pane with no foreground job (`skip_serializing_if`
/// on each), so both default rather than fail the envelope.
#[derive(Deserialize)]
struct ProcessInfo {
    #[serde(default)]
    foreground_process_group_id: Option<u32>,
    #[serde(default)]
    foreground_processes: Vec<ProcessEntry>,
}

#[derive(Deserialize)]
struct ProcessEntry {
    pid: u32,
    name: String,
    argv: Option<Vec<String>>,
}

/// `GET /api/v1/panes`'s answer.
#[derive(Serialize, ToSchema)]
pub(crate) struct PanesBody {
    ok: bool,
    herdr: HerdrState,
    panes: Vec<PaneEntry>,
}

/// What this route knows about herdr itself. `reason` is the one fixed sentence
/// naming the absent state, omitted when herdr is present.
#[derive(Serialize, ToSchema)]
pub(crate) struct HerdrState {
    present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// One pane, in herdr's own order, with its clauth sessions.
#[derive(Serialize, ToSchema)]
pub(crate) struct PaneEntry {
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    /// `terminal_title_stripped`, `null` when herdr has no title.
    #[schema(required = true)]
    title: Option<String>,
    /// The agent herdr associates with the pane, `null` when none.
    #[schema(required = true)]
    agent: Option<String>,
    agent_status: String,
    /// The pane's cwd, `null` when herdr has none.
    #[schema(required = true)]
    cwd: Option<String>,
    focused: bool,
    /// The `tokens.clauth` display tag, `null` when absent; never used to join.
    #[schema(required = true)]
    tag: Option<String>,
    /// The pane's foreground process group, `null` when its `process-info` did
    /// not answer (the pane closed between the two calls) or when the pane has
    /// no foreground job.
    #[schema(required = true)]
    foreground_process_group_id: Option<u32>,
    sessions: Vec<PaneSession>,
    /// The agent's own session id herdr detected in the pane (`agent_session`
    /// of kind `id`), the id `GET /api/v1/sessions/{id}` pages; `null` when
    /// herdr detected none.
    #[schema(required = true)]
    agent_session_id: Option<String>,
}

/// One clauth session running inside a pane.
#[derive(Serialize, ToSchema)]
pub(crate) struct PaneSession {
    session_id: String,
    /// The member the session currently holds, else its launch profile.
    profile: String,
    kind: SessionKind,
    follows_chain: bool,
    isolated: bool,
    /// The session's cwd, `null` when the row has none.
    #[schema(required = true)]
    cwd: Option<String>,
}

/// How a session's pid matched the pane.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionKind {
    /// The row's pid leads the pane's foreground process group (the pane's own
    /// `clauth start` or `clauth resume`), or it is a listed member that is a
    /// `clauth` running `start` or `resume` under a wrapper. On Windows herdr
    /// lists only the pane's agent root, so the supervisor is never seen there
    /// and no session joins (the herdr half of the bridge is linux + macOS).
    Session,
    /// Any other listed process inside the pane (a `claude` child, a `clauth
    /// mcp` in flight, a wrapper-launched `clauth` whose argv is unreadable).
    Delegate,
}

/// `GET /api/v1/panes` — every herdr pane with the clauth sessions inside it.
///
/// Joined on process ids, never on the `tokens.clauth` tag. Herdr absent
/// answers 200 with `present: false`, the one fixed sentence naming the state,
/// and no panes — never an error. At most `1 + N` herdr calls at 2 s each,
/// where `N` is the pane count.
#[utoipa::path(
    get,
    path = "/api/v1/panes",
    responses(
        (status = 200, description = "every herdr pane on this host with the clauth sessions running inside it, joined on process ids (the foreground process group leader is the pane's own session, as is a listed `clauth` running `start` or `resume` under a wrapper; any other listed process is a delegate; the display tag never joins; on Windows herdr lists only the pane's agent root, so no session joins there); at most 1 + N herdr calls at 2 s each", body = PanesBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn panes(ctx: &ApiContext, _: &Request, _: &Caller<'_>) -> Response {
    let rows = crate::live_sessions::list();
    let herdr_panes = match (ctx.herdr_probe)(&["pane", "list"], crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::NotInstalled => {
            return Response::serialize(200, &absent_body(NOT_INSTALLED));
        }
        HerdrProbeOut::Ran(None) => {
            return Response::serialize(200, &absent_body(NO_SERVER));
        }
        HerdrProbeOut::Ran(Some(out)) => {
            if !out.success {
                return Response::serialize(200, &absent_body(NO_SERVER));
            }
            match crate::herdr::parse_pane_list(&out.stdout) {
                None => return Response::serialize(200, &absent_body(NO_SERVER)),
                Some(panes) => panes,
            }
        }
    };
    let rows = newest_per_pid(&rows);
    let mut panes = Vec::with_capacity(herdr_panes.len());
    for pane in herdr_panes {
        match process_info(ctx, &pane.pane_id) {
            Some(info) => panes.push(join_pane(pane, info, &rows)),
            None => panes.push(pane_entry(pane, None, Vec::new())),
        }
    }
    Response::serialize(
        200,
        &PanesBody {
            ok: true,
            herdr: HerdrState {
                present: true,
                reason: None,
            },
            panes,
        },
    )
}

fn absent_body(reason: &str) -> PanesBody {
    PanesBody {
        ok: true,
        herdr: HerdrState {
            present: false,
            reason: Some(reason.to_string()),
        },
        panes: Vec::new(),
    }
}

/// Newest row wins per pid: a recycled pid can match a stale row and a live one
/// at once, and the dead row's profile must not show as a ghost session.
fn newest_per_pid(rows: &[LiveSession]) -> Vec<&LiveSession> {
    let mut newest: HashMap<u32, &LiveSession> = HashMap::new();
    for row in rows {
        match newest.entry(row.pid) {
            Entry::Occupied(mut slot) => {
                if row.started_at > slot.get().started_at {
                    slot.insert(row);
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(row);
            }
        }
    }
    newest.into_values().collect()
}

fn join_pane(pane: HerdrPane, info: ProcessInfo, rows: &[&LiveSession]) -> PaneEntry {
    let group_id = info.foreground_process_group_id;
    let processes = info.foreground_processes;

    let mut sessions = Vec::new();
    for row in rows {
        // The group leader is the pane's own session whatever its argv says: a
        // delegate child never leads the pane's group.
        let kind = if group_id == Some(row.pid) {
            SessionKind::Session
        } else {
            match processes.iter().find(|entry| entry.pid == row.pid) {
                Some(entry) if is_clauth_session(entry) => SessionKind::Session,
                Some(_) => SessionKind::Delegate,
                None => continue,
            }
        };
        sessions.push(PaneSession {
            session_id: row.session_id.clone(),
            profile: row
                .current_member
                .clone()
                .unwrap_or_else(|| row.start_profile.clone()),
            kind,
            follows_chain: row.follows_chain,
            isolated: row.isolated,
            cwd: row
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        });
    }
    sessions.sort_by(|a, b| {
        (!matches!(&a.kind, SessionKind::Session))
            .cmp(&!matches!(&b.kind, SessionKind::Session))
            .then_with(|| a.session_id.as_str().cmp(b.session_id.as_str()))
    });

    pane_entry(pane, group_id, sessions)
}

/// A listed member that is the pane's own session under a wrapper: a `clauth`
/// running one of the verbs that register a session row, `start` or `resume`.
/// The `.exe` stem strip is forward-compatibility only: herdr 0.9.0 lists no
/// `clauth.exe` on Windows (only the pane's agent root), so nothing reaches it.
fn is_clauth_session(entry: &ProcessEntry) -> bool {
    let stem = entry.name.strip_suffix(".exe").unwrap_or(&entry.name);
    stem == "clauth"
        && matches!(
            entry
                .argv
                .as_ref()
                .and_then(|argv| argv.get(1))
                .map(String::as_str),
            Some("start" | "resume")
        )
}

fn pane_entry(pane: HerdrPane, group_id: Option<u32>, sessions: Vec<PaneSession>) -> PaneEntry {
    PaneEntry {
        pane_id: pane.pane_id,
        workspace_id: pane.workspace_id,
        tab_id: pane.tab_id,
        title: pane.terminal_title_stripped,
        agent: pane.agent,
        agent_status: pane.agent_status,
        cwd: pane.cwd,
        focused: pane.focused,
        tag: pane.tokens.and_then(|tokens| tokens.clauth),
        foreground_process_group_id: group_id,
        sessions,
        agent_session_id: pane
            .agent_session
            .as_ref()
            .and_then(|session| session.session_id())
            .map(str::to_owned),
    }
}

/// One pane's `process-info`, or `None` when the call failed to answer (the
/// pane closed between the list and this call, say) or its stdout was not the
/// envelope.
fn process_info(ctx: &ApiContext, pane_id: &str) -> Option<ProcessInfo> {
    let args = ["pane", "process-info", "--pane", pane_id];
    let out = match (ctx.herdr_probe)(&args, crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::NotInstalled | HerdrProbeOut::Ran(None) => return None,
        HerdrProbeOut::Ran(Some(out)) => out,
    };
    if !out.success {
        return None;
    }
    parse_process_info(&out.stdout)
}

/// The envelope's `process_info`, or `None` when the stdout is not the envelope.
fn parse_process_info(stdout: &[u8]) -> Option<ProcessInfo> {
    serde_json::from_slice::<ProcessInfoEnvelope>(stdout)
        .ok()
        .map(|envelope| envelope.result.process_info)
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_panes.rs"]
mod tests;
