//! `POST /api/v1/sessions` — create a herdr tab in a cwd and start an agent in
//! its pane, behind the config key, the device's sessions grant, and the body
//! validation.
//!
//! Every daemon-side herdr spawn targets herdr's default session through the
//! context's seam ([`super::panes::PaneProbe`]), the one place that names the
//! binary and strips the session env, so no test runs a real herdr. herdr's
//! own detection and refusals are the truth the app shows: `agent start`'s
//! `agent_not_ready` is a created session blocked on startup, while an
//! unsupported kind or a hung start closes the tab and refuses.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::agent::{HERDR_REFUSED, error_code, output_for_log};
use super::devices::Tier;
use super::http::{Request, Response, sanitize_for_log};
use super::panes::{HerdrOut, HerdrProbeOut, NO_SERVER, NOT_INSTALLED};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::logline::logline;

/// The deadline `agent start`'s readiness wait runs to; every other call keeps
/// the seam's [`crate::herdr::PROBE_TIMEOUT`]. herdr's own `--timeout 60000`
/// answers first in the normal cases, so this only bounds a hung herdr.
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(65);

/// The fixed sentences; nothing off the wire reaches a body.
const SESSION_CREATION_OFF: &str = "session creation is off; set `session_creation = true` \
                                   under `[serve]` in profiles.toml to enable it";
const SESSIONS_GRANT_REQUIRED: &str = "this device lacks the sessions grant; run `clauth \
                                       devices allow-sessions <name>` on the host to grant it";
const CWD_NOT_ABSOLUTE: &str = "cwd must be an absolute path";
const CWD_NOT_A_DIRECTORY: &str = "cwd must name an existing directory";
const PROFILE_AND_KIND: &str = "name either a profile or a kind, never both";
const PROFILE_BAD_CHARS: &str =
    "profile must be letters, digits and - _ . @ + only, and can't start with '.'";
const KIND_BAD_SHAPE: &str = "kind must be 1..=32 ascii lowercase letters or digits";
const WORKSPACE_BAD_SHAPE: &str = "workspace must be 1..=32 chars of letters, digits or colons";
const PROFILE_NOT_FOUND: &str = "no stored claude or codex profile has that name";
const WORKSPACE_NOT_FOUND: &str = "no herdr workspace has that id";

/// The body `POST /api/v1/sessions` accepts.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SessionCreateBody {
    /// Absolute path to the directory the pane's shell starts in.
    cwd: String,
    /// A clauth profile (claude or codex roster); mutually exclusive with
    /// `kind`. Neither means bare `claude`.
    #[serde(default)]
    profile: Option<String>,
    /// A herdr agent kind; mutually exclusive with `profile`.
    #[serde(default)]
    kind: Option<String>,
    /// The herdr workspace the tab lands in, when named.
    #[serde(default)]
    workspace: Option<String>,
}

/// The answer a created session carries.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionCreated {
    ok: bool,
    workspace_id: String,
    tab_id: String,
    pane_id: String,
    /// herdr's `agent_status` read back after the agent call; `null` for the
    /// `pane run` arm (a clauth profile), whose readiness herdr detects later
    /// and `/events` carries.
    #[schema(required = true)]
    agent_status: Option<String>,
}

/// Which agent form the request named, for the audit line.
enum AgentForm<'a> {
    /// Bare `claude`, the globally linked account.
    Claude,
    /// `clauth start <profile>`.
    Profile(&'a str),
    /// `herdr agent start --kind <kind>`.
    Kind(&'a str),
}

impl AgentForm<'_> {
    /// The one-string form the audit line carries.
    fn for_log(&self) -> String {
        match self {
            AgentForm::Claude => "claude".to_string(),
            AgentForm::Profile(name) => format!("profile:{name}"),
            AgentForm::Kind(kind) => format!("kind:{kind}"),
        }
    }
}

/// What the agent call starts, once the form is known.
enum Start<'a> {
    /// `agent start --kind <kind>`.
    Kind(&'a str),
    /// `pane run <pane> clauth start <profile>`.
    Profile(&'a str),
}

/// `herdr tab create`'s success envelope, only the fields the answer needs.
#[derive(Deserialize)]
struct TabCreatedEnvelope {
    result: TabCreatedResult,
}

#[derive(Deserialize)]
struct TabCreatedResult {
    root_pane: RootPane,
}

#[derive(Deserialize)]
struct RootPane {
    pane_id: String,
    tab_id: String,
    workspace_id: String,
}

/// `herdr pane get`'s success envelope; the pane object is the same shape a
/// `pane list` entry carries.
#[derive(Deserialize)]
struct PaneGetEnvelope {
    result: PaneGetResult,
}

#[derive(Deserialize)]
struct PaneGetResult {
    pane: crate::herdr::HerdrPane,
}

/// The tab `tab create` left behind.
struct CreatedTab {
    pane_id: String,
    tab_id: String,
    workspace_id: String,
}

/// A herdr kind's shape: 1..=32 ASCII lowercase letters or digits, judged
/// before any spawn because the name lands in a herdr option slot.
fn valid_kind(kind: &str) -> bool {
    (1..=32).contains(&kind.len())
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// A workspace id's shape: 1..=32 chars of letters, digits or colons.
fn valid_workspace(workspace: &str) -> bool {
    (1..=32).contains(&workspace.len())
        && workspace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b':')
}

/// Whether a profile name is in the loaded claude roster or the codex roster,
/// case-insensitively.
fn profile_exists(name: &str, claude: &[crate::profile::ProfileName]) -> anyhow::Result<bool> {
    let in_claude = claude.iter().any(|n| n.eq_ignore_ascii_case(name));
    let in_codex = crate::codex_profiles::CodexState::load()?
        .profiles()
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name));
    Ok(in_claude || in_codex)
}

/// One `tab create` through the seam, its outcome reduced to the created tab
/// or the refusal to answer.
fn tab_create(
    ctx: &ApiContext,
    args: &[&str],
    device: &str,
    cwd: &str,
) -> Result<CreatedTab, Response> {
    let out = match (ctx.herdr_probe)(args, crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::NotInstalled => {
            return Err(Response::refused(503, "herdr_unavailable", NOT_INSTALLED));
        }
        HerdrProbeOut::Ran(None) => {
            logline!(
                "clauth api: device '{device}' tab create in cwd '{}' did not answer (deadline \
                 or spawn failure); a tab may exist unattributed",
                sanitize_for_log(cwd)
            );
            return Err(Response::refused(503, "herdr_unavailable", NO_SERVER));
        }
        HerdrProbeOut::Ran(Some(out)) => out,
    };
    if !out.success {
        return Err(match error_code(&out).as_deref() {
            Some("workspace_not_found") => {
                Response::refused(409, "workspace_not_found", WORKSPACE_NOT_FOUND)
            }
            Some("server_not_running") => Response::refused(503, "herdr_unavailable", NO_SERVER),
            _ => {
                logline!(
                    "clauth api: device '{device}' tab create refused by herdr: {}",
                    output_for_log(&out)
                );
                Response::refused(502, "herdr_refused", HERDR_REFUSED)
            }
        });
    }
    match serde_json::from_slice::<TabCreatedEnvelope>(&out.stdout)
        .ok()
        .map(|envelope| CreatedTab {
            pane_id: envelope.result.root_pane.pane_id,
            tab_id: envelope.result.root_pane.tab_id,
            workspace_id: envelope.result.root_pane.workspace_id,
        }) {
        Some(tab) => Ok(tab),
        None => {
            logline!(
                "clauth api: device '{device}' tab create answered an unparseable envelope: {}",
                output_for_log(&out)
            );
            Err(Response::refused(502, "herdr_refused", HERDR_REFUSED))
        }
    }
}

/// Read the pane's `agent_status` back after an `agent start`; `unknown` when
/// the read-back cannot be parsed.
fn agent_status(ctx: &ApiContext, pane_id: &str) -> String {
    match (ctx.herdr_probe)(&["pane", "get", pane_id], crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout,
            ..
        })) => serde_json::from_slice::<PaneGetEnvelope>(&stdout)
            .ok()
            .map(|envelope| envelope.result.pane.agent_status)
            .unwrap_or_else(|| "unknown".to_string()),
        _ => "unknown".to_string(),
    }
}

/// Close the tab a failed agent start left behind; the failure is logged, the
/// refusal stands.
fn close_tab(ctx: &ApiContext, tab_id: &str, device: &str) {
    match (ctx.herdr_probe)(&["tab", "close", tab_id], crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::Ran(Some(HerdrOut { success: true, .. })) => {}
        HerdrProbeOut::NotInstalled => {
            logline!(
                "clauth api: device '{device}' tab close '{tab}' failed: herdr is not installed",
                tab = sanitize_for_log(tab_id)
            );
        }
        HerdrProbeOut::Ran(None) => {
            logline!(
                "clauth api: device '{device}' tab close '{tab}' did not answer (deadline or spawn failure)",
                tab = sanitize_for_log(tab_id)
            );
        }
        HerdrProbeOut::Ran(Some(out)) => {
            logline!(
                "clauth api: device '{device}' tab close '{tab}' failed: {}",
                output_for_log(&out),
                tab = sanitize_for_log(tab_id)
            );
        }
    }
}

/// `POST /api/v1/sessions` — create a herdr pane running an agent.
#[utoipa::path(
    post,
    path = "/api/v1/sessions",
    request_body = SessionCreateBody,
    responses(
        (status = 200, description = "the pane was created and the agent started; `agent_status` is herdr's answer read back after the start, `null` for a `pane run` (a clauth profile)", body = SessionCreated),
        (status = 400, description = "the body held no parseable cwd, or a cwd that is not an absolute existing directory, both a profile and a kind, a profile whose name is not shaped like one, a kind outside 1..=32 ascii lowercase letters or digits, or a workspace outside 1..=32 chars of letters, digits or colons (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), a view-only device (`control_required`), session creation off in profiles.toml (`session_creation_off`), or a device without the sessions grant (`sessions_grant_required`)", body = ErrorBody),
        (status = 404, description = "the profile is not stored in the claude or codex roster (`profile_not_found`)", body = ErrorBody),
        (status = 409, description = "the named workspace, or the default when none is named, does not exist (`workspace_not_found`)", body = ErrorBody),
        (status = 502, description = "herdr refused the creation for another reason, recorded in daemon.log (`herdr_refused`)", body = ErrorBody),
        (status = 503, description = "herdr is not installed, or no server answered on its socket (`herdr_unavailable`)", body = ErrorBody),
        (status = 500, description = "the device list does not read, or profiles.toml or a roster does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn create(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    // Gate 1: the config key, read fresh per request (threat-model SC-3). A
    // load error refuses like the neighbouring routes refuse a device-list
    // load error, never as "off".
    let state = match crate::profile::load_app_state() {
        Ok(state) => state,
        Err(e) => {
            logline!("clauth api: session creation refused, profiles.toml does not read: {e:#}");
            return Response::error(500, "internal");
        }
    };
    if !state.serve.session_creation {
        return Response::refused(403, "session_creation_off", SESSION_CREATION_OFF);
    }

    let Some(device) = caller.device else {
        return Response::error(500, "internal");
    };
    // Gate 2: the control tier (already enforced by the table's Access::Control
    // row; re-checked so a hand-edited row is never read on its own) and the
    // per-device sessions grant.
    if device.tier != Tier::Control || !device.sessions {
        return Response::refused(403, "sessions_grant_required", SESSIONS_GRANT_REQUIRED);
    }
    let device = caller.device_for_log();

    let Ok(body) = serde_json::from_slice::<SessionCreateBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };
    if !std::path::Path::new(&body.cwd).is_absolute() {
        return Response::refused(400, "bad_request", CWD_NOT_ABSOLUTE);
    }
    if !std::fs::metadata(&body.cwd).is_ok_and(|meta| meta.is_dir()) {
        return Response::refused(400, "bad_request", CWD_NOT_A_DIRECTORY);
    }
    if body.profile.is_some() && body.kind.is_some() {
        return Response::refused(400, "bad_request", PROFILE_AND_KIND);
    }
    if let Some(kind) = &body.kind
        && !valid_kind(kind)
    {
        return Response::refused(400, "bad_request", KIND_BAD_SHAPE);
    }
    if let Some(workspace) = &body.workspace
        && !valid_workspace(workspace)
    {
        return Response::refused(400, "bad_request", WORKSPACE_BAD_SHAPE);
    }
    if let Some(profile) = &body.profile {
        if crate::actions::validate_name_chars(profile).is_err() {
            return Response::refused(400, "bad_request", PROFILE_BAD_CHARS);
        }
        match profile_exists(profile, &state.profiles) {
            Ok(true) => {}
            Ok(false) => return Response::refused(404, "profile_not_found", PROFILE_NOT_FOUND),
            Err(e) => {
                logline!(
                    "clauth api: session creation refused, the profile rosters do not read: {e:#}"
                );
                return Response::error(500, "internal");
            }
        }
    }

    let mut tab_args: Vec<&str> = vec!["tab", "create", "--cwd", &body.cwd, "--no-focus"];
    if let Some(workspace) = &body.workspace {
        tab_args.push("--workspace");
        tab_args.push(workspace);
    }
    let created = match tab_create(ctx, &tab_args, &device, &body.cwd) {
        Ok(tab) => tab,
        Err(response) => return response,
    };

    let form = if let Some(profile) = &body.profile {
        AgentForm::Profile(profile)
    } else if let Some(kind) = &body.kind {
        AgentForm::Kind(kind)
    } else {
        AgentForm::Claude
    };

    let start = match &form {
        AgentForm::Claude => Start::Kind("claude"),
        AgentForm::Kind(kind) => Start::Kind(kind),
        AgentForm::Profile(profile) => Start::Profile(profile),
    };

    let agent_status = match start {
        Start::Profile(profile) => {
            let args = ["pane", "run", &created.pane_id, "clauth", "start", profile];
            match drive_agent(ctx, &args, &device, crate::herdr::PROBE_TIMEOUT, "pane run") {
                Ok(()) => None,
                Err(response) => {
                    close_tab(ctx, &created.tab_id, &device);
                    return response;
                }
            }
        }
        Start::Kind(kind) => {
            let name = agent_name(&created.pane_id);
            let args = [
                "agent",
                "start",
                &name,
                "--kind",
                kind,
                "--pane",
                &created.pane_id,
                "--timeout",
                "60000",
            ];
            match drive_agent(ctx, &args, &device, AGENT_START_TIMEOUT, "agent start") {
                Ok(()) => Some(agent_status(ctx, &created.pane_id)),
                Err(response) => {
                    close_tab(ctx, &created.tab_id, &device);
                    return response;
                }
            }
        }
    };

    logline!(
        "clauth api: device '{}' created a session form='{}' cwd='{}' tab='{}' pane='{}'",
        device,
        sanitize_for_log(&form.for_log()),
        sanitize_for_log(&body.cwd),
        sanitize_for_log(&created.tab_id),
        sanitize_for_log(&created.pane_id)
    );
    Response::serialize(
        200,
        &SessionCreated {
            ok: true,
            workspace_id: created.workspace_id,
            tab_id: created.tab_id,
            pane_id: created.pane_id,
            agent_status,
        },
    )
}

/// One agent call through the seam, its outcome reduced to the answer. An
/// `agent_not_ready` exit is the created-but-blocked success the caller reads
/// back; every other non-zero exit is a refusal.
fn drive_agent(
    ctx: &ApiContext,
    args: &[&str],
    device: &str,
    deadline: Duration,
    what: &str,
) -> Result<(), Response> {
    let out = match (ctx.herdr_probe)(args, deadline) {
        HerdrProbeOut::NotInstalled => {
            return Err(Response::refused(503, "herdr_unavailable", NOT_INSTALLED));
        }
        HerdrProbeOut::Ran(None) => {
            logline!(
                "clauth api: device '{device}' {what} did not answer (deadline or spawn failure)"
            );
            return Err(Response::refused(502, "herdr_refused", HERDR_REFUSED));
        }
        HerdrProbeOut::Ran(Some(out)) => out,
    };
    let code = error_code(&out);
    if out.success || code.as_deref() == Some("agent_not_ready") {
        return Ok(());
    }
    let output = output_for_log(&out);
    let response = match code.as_deref() {
        Some("server_not_running") => Response::refused(503, "herdr_unavailable", NO_SERVER),
        _ => {
            logline!("clauth api: device '{device}' {what} refused by herdr: {output}");
            Response::refused(502, "herdr_refused", HERDR_REFUSED)
        }
    };
    Err(response)
}

/// herdr's agent name for the pane: `clauth-` plus the pane id, spelled to fit
/// herdr's rule (a lowercase letter first, then lowercase letters, digits, `-`
/// or `_`). herdr 0.9.1 numbers panes past `p9` with letters, uppercase among
/// them (`w1:pD`), which the rule refuses; an uppercase letter becomes `_` plus
/// its lowercase, so `pD` and `pd` stay distinct.
fn agent_name(pane_id: &str) -> String {
    let mut name = String::from("clauth-");
    for c in pane_id.chars() {
        match c {
            ':' => name.push('-'),
            c if c.is_ascii_uppercase() => {
                name.push('_');
                name.push(c.to_ascii_lowercase());
            }
            c => name.push(c),
        }
    }
    name
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_create.rs"]
mod tests;
