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

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::agent::{HERDR_REFUSED, error_code, output_for_log};
use super::devices::Tier;
use super::http::{Request, Response, sanitize_for_log};
use super::panes::{HerdrOut, HerdrProbeOut, NO_SERVER, NOT_INSTALLED};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::format::strip_ansi_csi;
use crate::logline::logline;
use crate::profile::ProfileName;

/// The deadline `agent start`'s readiness wait runs to; every other call keeps
/// the seam's [`crate::herdr::PROBE_TIMEOUT`]. herdr's own `--timeout 60000`
/// answers first in the normal cases, so this only bounds a hung herdr.
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(65);

/// The deadline a background launch runs to, beside [`AGENT_START_TIMEOUT`]: a
/// launch that never exits within it is killed and answers the same as one that
/// never answered.
const SESSION_LAUNCH_TIMEOUT: Duration = Duration::from_secs(65);

/// How long a background launch's pipes keep draining after the child exits: a
/// descendant (a daemonizing supervisor) holding one open can delay the answer
/// only this long past the exit, not to the run's bound.
const SESSION_LAUNCH_DRAIN: Duration = Duration::from_secs(2);

/// The bound the background session id read retries within. Still unread ->
/// `null`, never an error.
const SESSION_ID_READ_BOUND: Duration = Duration::from_secs(2);

/// The stderr lines one successful background launch may put in the daemon log:
/// more than the shared stores' notes, and short of letting a chatty child fill
/// the log per create.
const SESSION_LAUNCH_SAID_LINES: usize = 8;

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
const BACKGROUND_AND_KIND: &str =
    "background sessions run Claude Code; optionally pick a profile, not a kind";
const BACKGROUND_AND_WORKSPACE: &str =
    "background sessions do not open a herdr tab; drop the workspace";
const BACKGROUND_CODEX_PROFILE: &str =
    "background sessions run Claude Code; codex profiles are not supported";
const WITH_FALLBACK_AND_KIND: &str =
    "with_fallback follows a clauth profile's chain; pick a profile, not a kind";
const WITH_FALLBACK_NEEDS_PROFILE: &str = "with_fallback needs a profile to start on";
const PROMPT_NEEDS_BACKGROUND: &str =
    "prompt is for background sessions only; prompt a pane through its own prompt route";
const PROMPT_BAD_SHAPE: &str = "prompt must be non-empty and hold no NUL byte";
const BACKGROUND_BUSY: &str =
    "clauth is stopping this account's background service; retry in a moment";
const BACKGROUND_REFUSED: &str = "the background session did not start; see daemon.log";

/// The body `POST /api/v1/sessions` accepts.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SessionCreateBody {
    /// Absolute path to the directory the pane's shell, or the background
    /// session, starts in.
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
    /// Launch a Claude Code background session instead of a herdr pane; needs
    /// no herdr at all.
    #[serde(default)]
    background: bool,
    /// Follow the fallback chain, on either arm; needs `profile`.
    #[serde(default)]
    with_fallback: bool,
    /// The prompt for a background session, one argv word after `--bg`.
    #[serde(default)]
    prompt: Option<String>,
}

// Each answer's `type` tag is a one-variant enum, so a body cannot carry the
// other arm's value. Plain comments and `#[schema(inline)]` keep the served
// property a bare `type` + `enum`: a doc comment would become its
// `description`, and without `inline` it would be a `$ref`. The field came after
// both objects shipped, so it stays optional in the schema.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum PaneTag {
    Pane,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum BackgroundTag {
    Background,
}

/// The answer a created session carries.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionCreated {
    #[schema(inline, required = false)]
    r#type: PaneTag,
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

/// The answer a background creation carries: the job id off `claude --bg`'s
/// stdout, and the record's `sessionId` read back, `null` when it never read.
#[derive(Serialize, ToSchema)]
pub(crate) struct BackgroundSessionCreated {
    #[schema(inline, required = false)]
    r#type: BackgroundTag,
    ok: bool,
    background_id: String,
    #[schema(required = true)]
    session_id: Option<String>,
}

/// Which binary a background launch runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Launcher {
    /// `current_exe()` — clauth itself.
    Clauth,
    /// The crate's resolved `claude` CLI.
    Claude,
}

/// One background launch's bounded outcome.
pub(crate) struct LaunchOut {
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// The seam every background launch goes through; `None` = deadline or spawn
/// failure. The daemon passes the real spawn; a test records `(launcher, argv,
/// cwd)` and answers a fixture.
pub(crate) type SessionSpawn =
    Arc<dyn Fn(Launcher, &[&str], &Path) -> Option<LaunchOut> + Send + Sync>;

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

/// Which roster holds a profile name, resolved claude-first the way
/// `clauth start` resolves it, so a name in both rosters launches claude.
enum Roster {
    Claude,
    /// The codex roster's canonical casing, so the quarantine lookup reads the
    /// same path `clauth start` reads.
    Codex(String),
}

/// The roster that holds `name`, claude first then codex, case-insensitively;
/// `None` when neither holds it. One codex read answers the 404 and the launch
/// arm.
fn profile_roster(
    name: &str,
    claude: &[crate::profile::ProfileName],
) -> anyhow::Result<Option<Roster>> {
    if claude.iter().any(|n| n.eq_ignore_ascii_case(name)) {
        return Ok(Some(Roster::Claude));
    }
    if let Some(canonical) = crate::codex_profiles::CodexState::load()?.canonical_name(name) {
        return Ok(Some(Roster::Codex(canonical)));
    }
    Ok(None)
}

/// The 8-hex job id off `claude --bg`'s `backgrounded · <id>` line. The id is
/// the first word after the dot: a no-prompt launch follows it with a note on
/// the same line (`(idle — send a prompt to start)` on Claude Code 2.1.318).
/// `FORCE_COLOR` in the child's env wraps the id in colour codes (measured on
/// 2.1.319), so the lines are read with CSI sequences stripped.
fn background_id(stdout: &[u8]) -> Option<String> {
    // `claude --bg` separates the id with a middle dot (U+00B7); matched as its
    // UTF-8 bytes so the source stays ASCII.
    const PREFIX: &[u8] = b"backgrounded \xc2\xb7 ";
    let text = strip_ansi_csi(&String::from_utf8_lossy(stdout));
    for line in text.split('\n') {
        if !line.as_bytes().starts_with(PREFIX) {
            continue;
        }
        let Some(id) = line
            .get(PREFIX.len()..)
            .and_then(|rest| rest.split_whitespace().next())
        else {
            continue;
        };
        if id.len() == 8
            && id
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Some(id.to_string());
        }
    }
    None
}

/// Read the launched job's `sessionId`, retried within [`SESSION_ID_READ_BOUND`];
/// still unread answers `None`, never an error.
fn background_session_id(id: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + SESSION_ID_READ_BOUND;
    loop {
        if let Some(session) = crate::claude_jobs::job_session_id(id) {
            return Some(session);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Admit a claude profile in-process the way `clauth start <name>` would, on a
/// fresh config, and answer the canonical name — or the refusal to answer.
fn admit_claude_profile(body: &SessionCreateBody, profile: &str) -> Result<String, Response> {
    let config = match crate::profile::load_config() {
        Ok(config) => config,
        Err(e) => {
            logline!("clauth api: session creation refused, profiles.toml does not read: {e:#}");
            return Err(Response::error(500, "internal"));
        }
    };
    let Some(canonical) = config.canonical_name(profile) else {
        return Err(Response::refused(
            404,
            "profile_not_found",
            PROFILE_NOT_FOUND,
        ));
    };
    let name = ProfileName::from(canonical.clone());
    let mut claude_args = Vec::new();
    if body.background {
        claude_args.push("--bg".to_string());
        if let Some(prompt) = &body.prompt {
            claude_args.push("--".to_string());
            claude_args.push(prompt.clone());
        }
    }
    match crate::start::admit(
        &config,
        &name,
        crate::runtime::Isolation::Shared,
        body.with_fallback,
        &claude_args,
    ) {
        Ok(_) => Ok(canonical),
        Err(e) => match e.downcast_ref::<crate::start::StartRefusal>() {
            Some(refusal) => Err(refusal_response(refusal)),
            None => {
                logline!("clauth api: session creation refused: {e:#}");
                Err(Response::error(500, "internal"))
            }
        },
    }
}

/// The sessions-route answer for a typed start refusal: 409 with its code, or
/// the 500 for a refusal the route never reaches, which is logged before it
/// answers.
fn refusal_response(refusal: &crate::start::StartRefusal) -> Response {
    match refusal.code() {
        Some(code) => Response::refused(409, code, &refusal.to_string()),
        None => {
            logline!("clauth api: session creation refused: {refusal}");
            Response::error(500, "internal")
        }
    }
}

/// One background launch through the seam, reduced to its answer: 200 with the
/// job id and (best-effort) session id, 502 for a launch that fails, never
/// answers, or prints no job id, 503 when clauth is stopping that tree's
/// background service. The child's stderr reaches daemon.log only.
fn launch_background_session(
    ctx: &ApiContext,
    launcher: Launcher,
    argv: &[String],
    form: &str,
    body: &SessionCreateBody,
    device: &str,
) -> Response {
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let cwd = Path::new(&body.cwd);
    let Some(out) = (ctx.session_spawn)(launcher, &args, cwd) else {
        logline!(
            "clauth api: device '{device}' background launch did not answer (deadline or spawn \
             failure)"
        );
        return Response::refused(502, "background_refused", BACKGROUND_REFUSED);
    };
    if !out.success {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains(crate::runtime::BACKGROUND_STOP_MARKER) {
            logline!(
                "clauth api: device '{device}' background launch met a supervisor stop: {}",
                sanitize_for_log(stderr.trim_end())
            );
            return Response::refused(503, "background_busy", BACKGROUND_BUSY);
        }
        logline!(
            "clauth api: device '{device}' background launch refused: {}",
            sanitize_for_log(stderr.trim_end())
        );
        return Response::refused(502, "background_refused", BACKGROUND_REFUSED);
    }
    for said in String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(SESSION_LAUNCH_SAID_LINES)
    {
        logline!(
            "clauth api: device '{device}' background launch said: {}",
            sanitize_for_log(said.trim_end())
        );
    }
    let Some(id) = background_id(&out.stdout) else {
        let stdout = String::from_utf8_lossy(&out.stdout);
        logline!(
            "clauth api: device '{device}' background launch printed no job id: {}",
            sanitize_for_log(stdout.trim_end())
        );
        return Response::refused(502, "background_refused", BACKGROUND_REFUSED);
    };
    let session_id = background_session_id(&id);
    let session_field = session_id
        .as_deref()
        .map(|sid| format!("'{sid}'"))
        .unwrap_or_else(|| "null".to_string());
    let prompt_len = body.prompt.as_deref().map_or(0, str::len);
    logline!(
        "clauth api: device '{device}' created a background session form='{form}' \
         with_fallback={with_fallback} cwd='{cwd}' background_id='{id}' \
         session_id={session_field} prompt_len={prompt_len}",
        form = sanitize_for_log(form),
        with_fallback = body.with_fallback,
        cwd = sanitize_for_log(&body.cwd),
        id = id,
        session_field = session_field,
        prompt_len = prompt_len,
    );
    Response::serialize(
        200,
        &BackgroundSessionCreated {
            r#type: BackgroundTag::Background,
            ok: true,
            background_id: id,
            session_id,
        },
    )
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

/// `POST /api/v1/sessions` — create a herdr pane running an agent, or launch a
/// Claude Code background session.
#[utoipa::path(
    post,
    path = "/api/v1/sessions",
    request_body = SessionCreateBody,
    responses(
        (status = 200, description = "`type` names the arm: `pane`, the pane was created and the agent started (`agent_status` is herdr's answer read back after the start, `null` for a `pane run`), or `background`, a background session launched with the job id and the record's session id (`null` when it never read)", body = SessionCreated),
        (status = 400, description = "the body held no parseable cwd, or a cwd that is not an absolute existing directory, both a profile and a kind, a profile whose name is not shaped like one, a kind outside 1..=32 ascii lowercase letters or digits, a workspace outside 1..=32 chars of letters, digits or colons, background with a kind or workspace, background on a codex-roster profile, with_fallback with a kind or without a profile, prompt without background, or an empty or NUL-holding prompt (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), a view-only device (`control_required`), session creation off in profiles.toml (`session_creation_off`), or a device without the sessions grant (`sessions_grant_required`)", body = ErrorBody),
        (status = 404, description = "the profile is not stored in the claude or codex roster (`profile_not_found`)", body = ErrorBody),
        (status = 409, description = "the named workspace, or the default when none is named, does not exist (`workspace_not_found`); a disabled account (`profile_disabled`); a with_fallback refusal from clauth start (`with_fallback_unavailable`); a background launch refused on a copy host (`background_unavailable`); or a quarantined codex login (`login_quarantined`)", body = ErrorBody),
        (status = 502, description = "herdr refused the creation for another reason (`herdr_refused`), or the background session did not start (`background_refused`), recorded in daemon.log", body = ErrorBody),
        (status = 503, description = "herdr is not installed, or no server answered on its socket (`herdr_unavailable`); or clauth is stopping that account's background service (`background_busy`)", body = ErrorBody),
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
    if body.background && body.kind.is_some() {
        return Response::refused(400, "bad_request", BACKGROUND_AND_KIND);
    }
    if body.background && body.workspace.is_some() {
        return Response::refused(400, "bad_request", BACKGROUND_AND_WORKSPACE);
    }
    if body.with_fallback && body.kind.is_some() {
        return Response::refused(400, "bad_request", WITH_FALLBACK_AND_KIND);
    }
    if body.with_fallback && body.profile.is_none() {
        return Response::refused(400, "bad_request", WITH_FALLBACK_NEEDS_PROFILE);
    }
    if body.prompt.is_some() && !body.background {
        return Response::refused(400, "bad_request", PROMPT_NEEDS_BACKGROUND);
    }
    if let Some(prompt) = &body.prompt
        && (prompt.is_empty() || prompt.contains('\0'))
    {
        return Response::refused(400, "bad_request", PROMPT_BAD_SHAPE);
    }
    // Resolve the profile claude-first, then the codex roster, the way
    // `clauth start` does; one roster read answers the 404 and the launch arm.
    // The claude arm answers the canonical spelling, admitted in-process the
    // way the CLI admits it; the codex pane arm keeps the body's spelling.
    // `None` = bare claude.
    let profile_arg: Option<String> = match &body.profile {
        Some(profile) => {
            if crate::actions::validate_name_chars(profile).is_err() {
                return Response::refused(400, "bad_request", PROFILE_BAD_CHARS);
            }
            match profile_roster(profile, &state.profiles) {
                Ok(Some(Roster::Claude)) => match admit_claude_profile(&body, profile) {
                    Ok(name) => Some(name),
                    Err(response) => return response,
                },
                Ok(Some(Roster::Codex(canonical))) => {
                    if body.background {
                        return Response::refused(400, "bad_request", BACKGROUND_CODEX_PROFILE);
                    }
                    if body.with_fallback {
                        return refusal_response(&crate::start::StartRefusal::CodexWithFallback);
                    }
                    if let Err(refusal) = crate::codex_auth::refuse_if_quarantined(&canonical) {
                        return refusal_response(&refusal);
                    }
                    Some(canonical)
                }
                Ok(None) => return Response::refused(404, "profile_not_found", PROFILE_NOT_FOUND),
                Err(e) => {
                    logline!(
                        "clauth api: session creation refused, the profile rosters do not read: {e:#}"
                    );
                    return Response::error(500, "internal");
                }
            }
        }
        None => None,
    };

    if body.background {
        let (launcher, argv, form) = match &profile_arg {
            Some(name) => {
                let mut argv = vec!["start".to_string(), name.clone()];
                if body.with_fallback {
                    argv.push("--with-fallback".to_string());
                }
                argv.push("--".to_string());
                argv.push("--bg".to_string());
                if let Some(prompt) = &body.prompt {
                    argv.push("--".to_string());
                    argv.push(prompt.clone());
                }
                (Launcher::Clauth, argv, format!("profile:{name}"))
            }
            None => {
                let mut argv = vec!["--bg".to_string()];
                if let Some(prompt) = &body.prompt {
                    argv.push("--".to_string());
                    argv.push(prompt.clone());
                }
                (Launcher::Claude, argv, "claude".to_string())
            }
        };
        return launch_background_session(ctx, launcher, &argv, &form, &body, &device);
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

    let form = if let Some(profile) = &profile_arg {
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
            let mut args: Vec<&str> =
                vec!["pane", "run", &created.pane_id, "clauth", "start", profile];
            if body.with_fallback {
                args.push("--with-fallback");
            }
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

    let fallback = if body.with_fallback {
        " with_fallback=true"
    } else {
        ""
    };
    logline!(
        "clauth api: device '{}' created a session form='{}'{fallback} cwd='{}' tab='{}' pane='{}'",
        device,
        sanitize_for_log(&form.for_log()),
        sanitize_for_log(&body.cwd),
        sanitize_for_log(&created.tab_id),
        sanitize_for_log(&created.pane_id)
    );
    Response::serialize(
        200,
        &SessionCreated {
            r#type: PaneTag::Pane,
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

/// The stdout a background launch keeps; the job-id line sits in its first
/// lines.
const SESSION_LAUNCH_STDOUT_LIMIT: usize = 64 * 1024;

/// The production background spawn: build [`launch_command`] and run it bounded
/// by [`SESSION_LAUNCH_TIMEOUT`] on [`run_launch`]. `None` = spawn failure or
/// deadline. Never `start::run` in-process: that installs process-wide signal
/// handlers and calls `process::exit`.
pub(crate) fn real_session_spawn() -> SessionSpawn {
    Arc::new(|launcher, args, cwd| {
        if launcher == Launcher::Clauth && std::env::current_exe().is_err() {
            return None;
        }
        let mut command = launch_command(launcher, args, cwd);
        run_launch(&mut command, SESSION_LAUNCH_TIMEOUT)
    })
}

/// The one bounded run for a background launch, on
/// [`crate::gateway::run_bounded`]: both pipes drain on their own threads while
/// the child runs, the child is killed and reaped past `bound`, and a
/// descendant holding a pipe open holds the answer only until `bound`. `None` =
/// spawn failure, deadline, or a missing binary.
pub(crate) fn run_launch(command: &mut Command, bound: Duration) -> Option<LaunchOut> {
    let program = std::path::PathBuf::from(command.get_program());
    match crate::gateway::run_bounded(command, &program, bound, Some(SESSION_LAUNCH_STDOUT_LIMIT)) {
        Ok(crate::gateway::Bounded::Exited(exited)) => Some(LaunchOut {
            success: exited.status.success(),
            stdout: exited.stdout_within(SESSION_LAUNCH_DRAIN),
            stderr: exited.stderr_within(SESSION_LAUNCH_DRAIN),
        }),
        Ok(crate::gateway::Bounded::Missing | crate::gateway::Bounded::TimedOut) | Err(_) => None,
    }
}

/// The spawn a test context gets unless it installs one: fails loudly, so a
/// test that reaches the launch without a seam is a test that would otherwise
/// have run a real `clauth`/`claude`.
#[cfg(test)]
pub(crate) fn unspawnable_session() -> SessionSpawn {
    Arc::new(|_, _, _| {
        panic!("no session spawn installed for this test; use ApiContext::for_tests_with_spawn")
    })
}

/// The `Command` a background launch runs. `Clauth` is the installed path of
/// [`std::env::current_exe`] (a failure fails the launch, never PATH `clauth`);
/// `Claude` is the crate's resolver. Both run in `cwd` with stdin closed, both
/// pipes captured, and the session env a daemon may carry removed.
pub(crate) fn launch_command(launcher: Launcher, args: &[&str], cwd: &Path) -> Command {
    let mut command = match launcher {
        Launcher::Clauth => {
            // A failed `current_exe` is refused by [`real_session_spawn`]
            // before this is built; the empty program makes a stray direct
            // call fail at spawn with no PATH lookup of a plantable name.
            let program = match std::env::current_exe() {
                Ok(exe) => crate::platform::installed_exe_path(&exe),
                Err(_) => std::path::PathBuf::new(),
            };
            Command::new(program)
        }
        Launcher::Claude => crate::runtime::claude_command(),
    };
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::herdr::strip_session_env(&mut command);
    for key in [
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_PID",
        "CODEX_HOME",
        crate::hook_resume::RESTORE_MARKER_ENV,
        crate::hook_resume::DEGRADE_NOTICE_ENV,
        crate::hook_resume::DEGRADE_MARKER_ENV,
    ] {
        command.env_remove(key);
    }
    if launcher == Launcher::Claude {
        for key in crate::runtime::MANAGED_ENV_KEYS {
            command.env_remove(key);
        }
    }
    command
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_create.rs"]
mod tests;
