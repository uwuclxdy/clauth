//! `POST /api/v1/panes/{id}/prompt` and `POST /api/v1/panes/{id}/keys` — the
//! control half of driving a herdr agent from the app: a prompt through
//! `herdr agent prompt`, key presses through `herdr pane send-keys`.
//!
//! herdr's own detection and refusals are the truth the app shows: an agent
//! waiting on a prompt of its own refuses a new one (`agent_blocked`), and the
//! keys route is how it gets its `y`, `enter` or `esc`. Both calls go through
//! the context's herdr seam ([`super::panes::PaneProbe`]), the one place that
//! names the binary and strips the session env, so no test runs a real herdr.
//! The audit line names the pane and the size of what was sent, never its
//! content.

use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use super::http::{Request, Response, sanitize_for_log};
use super::panes::{
    HerdrOut, HerdrProbeOut, NO_SERVER, NOT_INSTALLED, PANE_NOT_FOUND, listed_panes,
};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::logline::logline;

/// The one field `POST /api/v1/panes/{id}/prompt` accepts.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PromptBody {
    /// The prompt, non-empty.
    text: String,
}

/// The one field `POST /api/v1/panes/{id}/keys` accepts.
#[derive(Deserialize, ToSchema)]
pub(crate) struct KeysBody {
    /// herdr's key names in press order, 1..=32 of them, each 1..=32 chars of
    /// letters, digits, `-`, `+` and `_` (`y`, `enter`, `esc`, `ctrl+c`);
    /// herdr decides which names exist.
    keys: Vec<String>,
}

/// The answer both routes carry once herdr accepted the request.
#[derive(Serialize, ToSchema)]
pub(crate) struct AgentOk {
    ok: bool,
}

/// A herdr pane id's shape: `<workspace>:<pane>`, each half opening with an
/// ASCII alphanumeric and continuing with alphanumerics, `-` or `_`
/// (`w1N:p19`). Judged before any spawn, because the id lands in herdr's
/// TARGET slot, where a leading `-` is an option to herdr's parser: `--help`
/// and `-h` print help at exit 0 (measured on 0.9.0) and would read as a
/// prompt delivered. Anything else herdr would refuse by name, so the answer
/// is the same 404 it would give.
fn valid_pane_id(id: &str) -> bool {
    let Some((workspace, pane)) = id.split_once(':') else {
        return false;
    };
    let half = |half: &str| {
        half.bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
            && half
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    half(workspace) && half(pane)
}

/// Bounds on a keys body: the shape is validated here, the names by herdr.
const KEYS_MAX: usize = 32;
const KEY_MAX_CHARS: usize = 32;

/// The fixed sentences; nothing off the wire reaches a body.
const AGENT_BLOCKED: &str =
    "the agent is waiting on a prompt of its own; answer it with keys or the terminal stream";
pub(crate) const HERDR_REFUSED: &str = "herdr refused the request; see daemon.log";

/// herdr's error envelope (measured 2026-09-17, herdr 0.9.0, the streams
/// separated): every one is printed on STDERR with an empty stdout at exit 1,
/// a refused request (`{"error":{"code":"agent_not_found","message":"agent
/// target w9:p99 not found"},"id":"cli:agent:prompt"}`) and a server it
/// cannot reach (`{"id":"cli:pane:list","error":{"code":"server_not_running",
/// "message":"no herdr server is running at …"}}`) alike. Only the code is
/// read; the message reaches the log alone, sanitized.
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    code: String,
}

/// The envelope's code off whichever stream carries one. stdout is tried
/// first only because it is cheap: a success never coexists with an error,
/// and every envelope herdr emits sits on stderr.
pub(crate) fn error_code(out: &HerdrOut) -> Option<String> {
    [&out.stdout, &out.stderr].into_iter().find_map(|stream| {
        serde_json::from_slice::<ErrorEnvelope>(stream)
            .ok()
            .map(|envelope| envelope.error.code)
    })
}

/// Why herdr did not do it, in the terms both routes answer.
enum Refusal {
    /// herdr knows no such pane, or no agent in it.
    PaneNotFound,
    /// The agent is waiting on a prompt of its own; carries herdr's output
    /// sanitized for the log, for the route that has no answer of its own for
    /// this.
    AgentBlocked(String),
    /// herdr itself is absent, with the fixed sentence naming the state.
    Unavailable(&'static str),
    /// Any other non-zero exit; carries herdr's output, sanitized for the log.
    Herdr(String),
}

/// One herdr call through the seam, its outcome reduced to the answer. A
/// success's stdout is never read: the exit status is the whole answer.
fn drive(ctx: &ApiContext, args: &[&str]) -> Result<(), Refusal> {
    match (ctx.herdr_probe)(args, crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::NotInstalled => Err(Refusal::Unavailable(NOT_INSTALLED)),
        HerdrProbeOut::Ran(None) => Err(Refusal::Unavailable(NO_SERVER)),
        HerdrProbeOut::Ran(Some(HerdrOut { success: true, .. })) => Ok(()),
        HerdrProbeOut::Ran(Some(out)) => match error_code(&out).as_deref() {
            Some("agent_not_found" | "pane_not_found") => Err(Refusal::PaneNotFound),
            Some("server_not_running") => Err(Refusal::Unavailable(NO_SERVER)),
            Some("agent_blocked") => Err(Refusal::AgentBlocked(output_for_log(&out))),
            _ => Err(Refusal::Herdr(output_for_log(&out))),
        },
    }
}

/// What a refused call printed, for one log line: stderr when stdout is
/// empty (herdr's every envelope), else stdout, through the sanitizer.
pub(crate) fn output_for_log(out: &HerdrOut) -> String {
    let stream = if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    sanitize_for_log(&String::from_utf8_lossy(stream))
}

/// The answer a refusal maps to. Only the catch-all logs: its status names
/// the log as the place the reason went, while the others say it themselves.
fn refuse(refusal: Refusal, caller: &Caller<'_>, pane: &str, what: &str) -> Response {
    match refusal {
        Refusal::PaneNotFound => Response::refused(404, "pane_not_found", PANE_NOT_FOUND),
        Refusal::AgentBlocked(_) => Response::refused(409, "agent_blocked", AGENT_BLOCKED),
        Refusal::Unavailable(reason) => Response::refused(503, "herdr_unavailable", reason),
        Refusal::Herdr(output) => {
            logline!(
                "clauth api: device '{}' {what} on pane '{}' refused by herdr: {output}",
                caller.device_for_log(),
                sanitize_for_log(pane)
            );
            Response::refused(502, "herdr_refused", HERDR_REFUSED)
        }
    }
}

/// `POST /api/v1/panes/{id}/prompt` — submit a prompt to the agent in a pane.
///
/// `herdr agent prompt <pane> <text>` without `--wait`: the answer says herdr
/// accepted the submission, and the events stream carries what the agent does
/// with it.
#[utoipa::path(
    post,
    path = "/api/v1/panes/{id}/prompt",
    params(
        ("id" = String, Path, description = "herdr's pane id, as `GET /api/v1/panes` lists it")
    ),
    request_body = PromptBody,
    responses(
        (status = 200, description = "herdr accepted the prompt for the agent in that pane", body = AgentOk),
        (status = 400, description = "the body held no non-empty text, or a text carrying a NUL (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "no pane with that id in herdr's default session, or no agent in it; an id not shaped like herdr's workspace:pane ids (each half an alphanumeric, then alphanumerics, dashes or underscores) answers this before herdr is asked (`pane_not_found`)", body = ErrorBody),
        (status = 409, description = "the agent is waiting on a prompt of its own (`agent_blocked`)", body = ErrorBody),
        (status = 502, description = "herdr refused the prompt for another reason, recorded in daemon.log (`herdr_refused`)", body = ErrorBody),
        (status = 503, description = "herdr is not installed, or no server answered on its socket (`herdr_unavailable`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn prompt(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Some(pane) = caller.target else {
        return Response::error(500, "internal");
    };
    if !valid_pane_id(pane) {
        return Response::refused(404, "pane_not_found", PANE_NOT_FOUND);
    }
    let Ok(body) = serde_json::from_slice::<PromptBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };
    // A NUL cannot be an argv byte: the spawn would fail and read as herdr
    // being absent, so it is the bad input it is.
    if body.text.is_empty() || body.text.contains('\0') {
        return Response::error(400, "bad_request");
    }
    match drive(ctx, &["agent", "prompt", pane, &body.text]) {
        Ok(()) => {
            logline!(
                "clauth api: device '{}' prompted pane '{}' text_len={}",
                caller.device_for_log(),
                sanitize_for_log(pane),
                body.text.len()
            );
            Response::serialize(200, &AgentOk { ok: true })
        }
        Err(refusal) => refuse(refusal, caller, pane, "prompt"),
    }
}

/// One key name's shape: herdr's vocabulary is letters, digits, `-`, `+` and
/// `_` (`ctrl+c`, `shift-tab`), so anything else is refused here and the
/// names themselves are herdr's to judge.
fn valid_key(key: &str) -> bool {
    (1..=KEY_MAX_CHARS).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'+' | b'_'))
}

/// `POST /api/v1/panes/{id}/keys` — press keys in a pane, the way a blocked
/// agent gets its answer without the terminal mirror.
#[utoipa::path(
    post,
    path = "/api/v1/panes/{id}/keys",
    params(
        ("id" = String, Path, description = "herdr's pane id, as `GET /api/v1/panes` lists it")
    ),
    request_body = KeysBody,
    responses(
        (status = 200, description = "herdr pressed the keys in that pane, in order", body = AgentOk),
        (status = 400, description = "the body held no key list of 1 to 32 names, each 1 to 32 chars of letters, digits, dash, plus and underscore (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "no pane with that id in herdr's default session; an id not shaped like herdr's workspace:pane ids (each half an alphanumeric, then alphanumerics, dashes or underscores) answers this before herdr is asked (`pane_not_found`)", body = ErrorBody),
        (status = 502, description = "herdr refused the keys, a name it does not know included, recorded in daemon.log (`herdr_refused`)", body = ErrorBody),
        (status = 503, description = "herdr is not installed, or no server answered on its socket (`herdr_unavailable`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn keys(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Some(pane) = caller.target else {
        return Response::error(500, "internal");
    };
    if !valid_pane_id(pane) {
        return Response::refused(404, "pane_not_found", PANE_NOT_FOUND);
    }
    let Ok(body) = serde_json::from_slice::<KeysBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };
    if !(1..=KEYS_MAX).contains(&body.keys.len()) || !body.keys.iter().all(|key| valid_key(key)) {
        return Response::error(400, "bad_request");
    }
    let mut args: Vec<&str> = vec!["pane", "send-keys", pane];
    args.extend(body.keys.iter().map(String::as_str));
    match drive(ctx, &args) {
        Ok(()) => {
            logline!(
                "clauth api: device '{}' sent keys to pane '{}' keys={}",
                caller.device_for_log(),
                sanitize_for_log(pane),
                body.keys.len()
            );
            Response::serialize(200, &AgentOk { ok: true })
        }
        // `agent_blocked` is `agent prompt`'s refusal, not `pane send-keys`'s
        // (keys are how a blocked agent gets answered), so here it is a herdr
        // answer this route does not know, logged like any other.
        Err(Refusal::AgentBlocked(output)) => {
            refuse(Refusal::Herdr(output), caller, pane, "send-keys")
        }
        Err(refusal) => refuse(refusal, caller, pane, "send-keys"),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_agent.rs"]
mod tests;

/// The answer both session-keyed command routes carry once the command reached
/// its target: `via` names the mod or the herdr fallback.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionDriveOk {
    ok: bool,
    via: &'static str,
}

/// The answer the ask route carries once the answer reached a live mod.
#[derive(Serialize, ToSchema)]
pub(crate) struct AnswerOk {
    ok: bool,
}

/// The empty-object body `POST /api/v1/sessions/{id}/interrupt` documents. The
/// route reads no field, so any JSON object (`{}` or with fields) is accepted
/// and nothing is taken off it; the object-only shape is enforced in the route
/// (B#12).
#[derive(Deserialize, ToSchema)]
pub(crate) struct InterruptBody {}

/// The one fixed sentence each no-reach state carries; nothing off the wire.
const SESSION_NOT_LIVE: &str =
    "no live session with that id: no mod instance and no herdr pane on it";
/// The ask route's own no-reach sentence: it is mod-only, so a session whose
/// mod died while its herdr pane lives has no reach here (B#18).
const ASK_NOT_LIVE: &str = "no live mod instance with that id";
const SESSION_AMBIGUOUS: &str =
    "two live sessions share that id; refused rather than handed to either";
#[cfg(unix)]
const SESSION_BUSY: &str = "the mod's command queue is full; retry after its next poll";
#[cfg(unix)]
const ASK_CLOSED: &str = "that ask is already answered or closed";
#[cfg(unix)]
const ASK_NOT_FOUND: &str = "no open ask with that id on that session";

/// Where the herdr fallback landed for one session id.
enum PaneFallback {
    /// Exactly one live pane carries the id as its `agent_session_id`.
    Pane(String),
    /// None: the session has no pane to reach.
    None,
    /// Several panes carry the id, and neither is singled out.
    Ambiguous,
}

/// The live pane whose `agent_session_id` equals `id`, through at most one
/// bounded `herdr pane list` call. herdr absent or unanswerable reads as no
/// pane, which the routes answer `session_not_live` (SB-16).
fn session_pane(ctx: &ApiContext, id: &str) -> PaneFallback {
    let Ok(panes) = listed_panes(ctx) else {
        return PaneFallback::None;
    };
    let mut matching = panes
        .into_iter()
        .filter(|pane| pane.agent_session_id() == Some(id))
        .map(|pane| pane.pane_id);
    match (matching.next(), matching.next()) {
        (Some(pane), None) => PaneFallback::Pane(pane),
        (Some(_), Some(_)) => PaneFallback::Ambiguous,
        _ => PaneFallback::None,
    }
}

/// `POST /api/v1/sessions/{id}/prompt` — prompt the session's mod, else the
/// herdr pane joined on the session id. Control tier; the mod half is unix-only
/// (Windows has no bridge), while the herdr fallback still applies on Windows.
#[utoipa::path(
    post,
    path = "/api/v1/sessions/{id}/prompt",
    params(
        ("id" = String, Path, description = "the session id, as `GET /api/v1/sessions` lists it")
    ),
    request_body = PromptBody,
    responses(
        (status = 200, description = "the prompt reached the session's mod, or the herdr pane joined on the session id", body = SessionDriveOk),
        (status = 400, description = "the body held no non-empty text, or a text carrying a NUL (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "no live session with that id (`session_not_live`), or the herdr fallback's pane is gone (`pane_not_found`)", body = ErrorBody),
        (status = 409, description = "two live sessions share that id (`session_ambiguous`), or the herdr fallback's agent is blocked (`agent_blocked`)", body = ErrorBody),
        (status = 502, description = "herdr refused the fallback prompt for another reason, recorded in daemon.log (`herdr_refused`)", body = ErrorBody),
        (status = 503, description = "the herdr fallback's pane list answered, but no herdr server answered the prompt (`herdr_unavailable`), or the mod's command queue is full (`session_busy`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn session_prompt(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Some(id) = caller.target else {
        return Response::error(500, "internal");
    };
    let Ok(body) = serde_json::from_slice::<PromptBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };
    if body.text.is_empty() || body.text.contains('\0') {
        return Response::error(400, "bad_request");
    }
    #[cfg(unix)]
    {
        if let Some(hub) = &ctx.bridge {
            match hub.deliver(id, json!({ "kind": "prompt", "text": body.text })) {
                crate::daemon::bridge::ModDelivery::Delivered => {
                    logline!(
                        "clauth api: device '{}' prompted session '{}' via mod text_len={}",
                        caller.device_for_log(),
                        sanitize_for_log(id),
                        body.text.len()
                    );
                    return Response::serialize(
                        200,
                        &SessionDriveOk {
                            ok: true,
                            via: "mod",
                        },
                    );
                }
                crate::daemon::bridge::ModDelivery::Ambiguous => {
                    logline!(
                        "clauth api: device '{}' refused on session '{}': ambiguous",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::refused(409, "session_ambiguous", SESSION_AMBIGUOUS);
                }
                crate::daemon::bridge::ModDelivery::Busy => {
                    logline!(
                        "clauth api: device '{}' refused on session '{}': busy",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::refused(503, "session_busy", SESSION_BUSY);
                }
                crate::daemon::bridge::ModDelivery::NoMod => {}
            }
        }
    }
    match session_pane(ctx, id) {
        PaneFallback::None => Response::refused(404, "session_not_live", SESSION_NOT_LIVE),
        PaneFallback::Ambiguous => {
            logline!(
                "clauth api: device '{}' refused on session '{}': ambiguous",
                caller.device_for_log(),
                sanitize_for_log(id)
            );
            Response::refused(409, "session_ambiguous", SESSION_AMBIGUOUS)
        }
        PaneFallback::Pane(pane) => {
            match drive(ctx, &["agent", "prompt", pane.as_str(), body.text.as_str()]) {
                Ok(()) => {
                    logline!(
                        "clauth api: device '{}' prompted session '{}' via herdr text_len={}",
                        caller.device_for_log(),
                        sanitize_for_log(id),
                        body.text.len()
                    );
                    Response::serialize(
                        200,
                        &SessionDriveOk {
                            ok: true,
                            via: "herdr",
                        },
                    )
                }
                Err(refusal) => refuse(refusal, caller, &pane, "prompt"),
            }
        }
    }
}

/// `POST /api/v1/sessions/{id}/interrupt` — interrupt the session's mod, else
/// send `esc` to the herdr pane joined on the session id.
#[utoipa::path(
    post,
    path = "/api/v1/sessions/{id}/interrupt",
    params(
        ("id" = String, Path, description = "the session id, as `GET /api/v1/sessions` lists it")
    ),
    request_body = InterruptBody,
    responses(
        (status = 200, description = "the interrupt reached the session's mod, or the herdr pane joined on the session id", body = SessionDriveOk),
        (status = 400, description = "the body is neither empty nor a JSON object (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "no live session with that id (`session_not_live`), or the herdr fallback's pane is gone (`pane_not_found`)", body = ErrorBody),
        (status = 409, description = "two live sessions share that id (`session_ambiguous`)", body = ErrorBody),
        (status = 502, description = "herdr refused the fallback keys for another reason, recorded in daemon.log (`herdr_refused`)", body = ErrorBody),
        (status = 503, description = "the herdr fallback's pane list answered, but no herdr server answered the keys (`herdr_unavailable`), or the mod's command queue is full (`session_busy`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn session_interrupt(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    // The documented body is `{}`; an empty body is accepted too (B#26), and
    // anything that is not a JSON object (a scalar, an array) is refused before
    // any work (B#12).
    if !req.body.is_empty()
        && !serde_json::from_slice::<serde_json::Value>(&req.body)
            .is_ok_and(|value| value.is_object())
    {
        return Response::error(400, "bad_request");
    }
    let Some(id) = caller.target else {
        return Response::error(500, "internal");
    };
    #[cfg(unix)]
    {
        if let Some(hub) = &ctx.bridge {
            match hub.deliver(id, json!({ "kind": "interrupt" })) {
                crate::daemon::bridge::ModDelivery::Delivered => {
                    logline!(
                        "clauth api: device '{}' interrupted session '{}' via mod",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::serialize(
                        200,
                        &SessionDriveOk {
                            ok: true,
                            via: "mod",
                        },
                    );
                }
                crate::daemon::bridge::ModDelivery::Ambiguous => {
                    logline!(
                        "clauth api: device '{}' refused on session '{}': ambiguous",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::refused(409, "session_ambiguous", SESSION_AMBIGUOUS);
                }
                crate::daemon::bridge::ModDelivery::Busy => {
                    logline!(
                        "clauth api: device '{}' refused on session '{}': busy",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::refused(503, "session_busy", SESSION_BUSY);
                }
                crate::daemon::bridge::ModDelivery::NoMod => {}
            }
        }
    }
    match session_pane(ctx, id) {
        PaneFallback::None => Response::refused(404, "session_not_live", SESSION_NOT_LIVE),
        PaneFallback::Ambiguous => {
            logline!(
                "clauth api: device '{}' refused on session '{}': ambiguous",
                caller.device_for_log(),
                sanitize_for_log(id)
            );
            Response::refused(409, "session_ambiguous", SESSION_AMBIGUOUS)
        }
        PaneFallback::Pane(pane) => {
            match drive(ctx, &["pane", "send-keys", pane.as_str(), "esc"]) {
                Ok(()) => {
                    logline!(
                        "clauth api: device '{}' interrupted session '{}' via herdr",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    Response::serialize(
                        200,
                        &SessionDriveOk {
                            ok: true,
                            via: "herdr",
                        },
                    )
                }
                // `agent_blocked` is `agent prompt`'s refusal, not `pane
                // send-keys`'s (keys are how a blocked agent gets answered), so
                // here it is a herdr answer this route does not know.
                Err(Refusal::AgentBlocked(output)) => {
                    refuse(Refusal::Herdr(output), caller, &pane, "send-keys")
                }
                Err(refusal) => refuse(refusal, caller, &pane, "send-keys"),
            }
        }
    }
}

/// The one `decision` the ask answer takes: `allow` or `deny`, so the served
/// schema is an enum rather than a free string (B#25).
#[derive(Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Allow,
    Deny,
}

impl Decision {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// One answer body: exactly one of `decision` (`allow` or `deny`) or `answers`
/// (an object), never both, never neither.
#[derive(Deserialize, ToSchema)]
struct AnswerBody {
    #[serde(default)]
    decision: Option<Decision>,
    #[serde(default)]
    answers: Option<serde_json::Value>,
}

/// `POST /api/v1/sessions/{id}/asks/{ask}` — answer a mod-held ask. The first
/// answer wins and closes the ask, desk or device alike (SB-7, SB-19).
#[utoipa::path(
    post,
    path = "/api/v1/sessions/{id}/asks/{ask}",
    params(
        ("id" = String, Path, description = "the session id, as `GET /api/v1/sessions` lists it"),
        ("ask" = String, Path, description = "the ask id the mod minted and posted with `ask_opened`")
    ),
    request_body = AnswerBody,
    responses(
        (status = 200, description = "the answer reached the session's live mod and closes the ask", body = AnswerOk),
        (status = 400, description = "the body names neither, or both, of decision and answers, or a decision other than allow or deny (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "no live session with that id (`session_not_live`), or no open ask with that id (`ask_not_found`)", body = ErrorBody),
        (status = 409, description = "two live sessions share that id (`session_ambiguous`), or the ask is already answered (`ask_closed`)", body = ErrorBody),
        (status = 503, description = "the mod's command queue is full (`session_busy`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn session_answer(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Some(id) = caller.target else {
        return Response::error(500, "internal");
    };
    let Some(ask) = caller.ask else {
        return Response::error(500, "internal");
    };
    let Ok(body) = serde_json::from_slice::<AnswerBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };
    // Exactly one form, and its value the one shape the mod accepts.
    let (command, audit_tail) = match (body.decision.as_ref(), body.answers) {
        (Some(decision), None) => (
            json!({ "kind": "answer", "ask": ask, "decision": decision }),
            format!("decision={}", decision.as_str()),
        ),
        (None, Some(answers)) if answers.is_object() => (
            json!({ "kind": "answer", "ask": ask, "answers": answers }),
            format!(
                "answers={}",
                answers.as_object().map_or(0, |object| object.len())
            ),
        ),
        _ => return Response::error(400, "bad_request"),
    };
    #[cfg(unix)]
    {
        if let Some(hub) = &ctx.bridge {
            match hub.answer(id, ask, command) {
                crate::daemon::bridge::AskOutcome::Queued => {
                    logline!(
                        "clauth api: device '{}' answered ask '{}' on session '{}' {}",
                        caller.device_for_log(),
                        sanitize_for_log(ask),
                        sanitize_for_log(id),
                        audit_tail
                    );
                    return Response::serialize(200, &AnswerOk { ok: true });
                }
                crate::daemon::bridge::AskOutcome::Ambiguous => {
                    logline!(
                        "clauth api: device '{}' refused on session '{}': ambiguous",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::refused(409, "session_ambiguous", SESSION_AMBIGUOUS);
                }
                crate::daemon::bridge::AskOutcome::Busy => {
                    logline!(
                        "clauth api: device '{}' refused on session '{}': busy",
                        caller.device_for_log(),
                        sanitize_for_log(id)
                    );
                    return Response::refused(503, "session_busy", SESSION_BUSY);
                }
                crate::daemon::bridge::AskOutcome::AskClosed => {
                    return Response::refused(409, "ask_closed", ASK_CLOSED);
                }
                crate::daemon::bridge::AskOutcome::AskNotFound => {
                    return Response::refused(404, "ask_not_found", ASK_NOT_FOUND);
                }
                crate::daemon::bridge::AskOutcome::NoSession => {
                    return Response::refused(404, "session_not_live", ASK_NOT_LIVE);
                }
            }
        }
    }
    // Windows has no mod bridge (SB-1/MB-9): the body is still validated, but
    // the mod-only ask route never holds an ask, so it reads session_not_live.
    #[cfg(not(unix))]
    let _ = (&ctx, &id, &command, &audit_tail);
    Response::refused(404, "session_not_live", ASK_NOT_LIVE)
}
