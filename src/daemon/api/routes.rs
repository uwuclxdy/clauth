//! Route table: read the feed, switch the account. Nothing else.
//!
//! The narrow surface is the design. A switch is the one mutation the daemon
//! already performs unattended, so exposing it adds no capability the fallback
//! chain does not have; everything that needs a human — a diverged live login,
//! an unprovable identity — is refused here exactly as it is refused for the
//! scheduler and the MCP tool, and the TUI stays the only place to resolve it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::actions::switch_profile_noninteractive;
use crate::daemon::build_status;
use crate::lock::StateLockTimeout;
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::oauth;
use crate::profile::ConfigHandle;

use super::http::{Request, Response, sanitize_for_log};
use super::token::AuthToken;

/// Everything a request handler is allowed to touch.
pub(crate) struct ApiContext {
    pub(crate) config: ConfigHandle,
    /// `~/.clauth/status.json` — the feed the main loop rewrites each tick.
    pub(crate) status_path: PathBuf,
    pub(crate) token: AuthToken,
    /// One in-flight `POST /v1/switch` at a time. See [`rank::ApiSwitch`].
    pub(crate) switch_gate: RankedMutex<(), rank::ApiSwitch>,
}

impl ApiContext {
    pub(crate) fn new(config: ConfigHandle, status_path: PathBuf, token: AuthToken) -> Arc<Self> {
        Arc::new(Self {
            config,
            status_path,
            token,
            switch_gate: RankedMutex::new(()),
        })
    }
}

/// Authenticate, then dispatch. Every route requires the bearer token,
/// including health: an unauthenticated caller still learns the daemon is alive
/// (it answers 401 rather than refusing the connection), which is all a liveness
/// probe needs, and nothing else leaks — not the version, not an account name.
pub(crate) fn handle(ctx: &ApiContext, req: &Request) -> Response {
    if !req.bearer.as_deref().is_some_and(|t| ctx.token.verify(t)) {
        return Response::unauthorized();
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/health") => health(),
        ("GET", "/v1/status") => status(ctx, req),
        ("POST", "/v1/switch") => switch(ctx, req),
        // A known path reached with the wrong method is 405, so a client with a
        // typo'd verb gets told which half is wrong.
        (_, "/v1/health" | "/v1/status") => Response::error(405, "method_not_allowed"),
        (_, "/v1/switch") => Response::error(405, "method_not_allowed"),
        _ => Response::error(404, "not_found"),
    }
}

fn health() -> Response {
    Response::json(
        200,
        &serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "schema": crate::daemon::SCHEMA_VERSION,
        }),
    )
}

/// `GET /v1/status` — the same body `~/.clauth/status.json` carries.
///
/// Served straight off disk in the common case. The main loop already rewrites
/// that file atomically every tick, so passing the bytes through takes no lock,
/// duplicates no serialization, and cannot drift from the documented schema.
/// `?all=1` and a missing file both fall back to building a body here, which is
/// the same `build_status` the file itself came from.
fn status(ctx: &ApiContext, req: &Request) -> Response {
    let include_disabled = req.flag("all");
    if !include_disabled && let Ok(body) = std::fs::read(&ctx.status_path) {
        return Response::raw_json(200, body);
    }
    // No file yet (the daemon is still in its first tick), or the caller asked
    // for the disabled accounts the published feed always hides.
    let (snapshot, interval) = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        (cfg.clone(), cfg.state.refresh_interval_ms)
    };
    let body = build_status(&snapshot, interval, None, include_disabled);
    match serde_json::to_vec(&body) {
        Ok(bytes) => Response::raw_json(200, bytes),
        Err(e) => {
            logline!("clauth api: failed to serialize a status body: {e}");
            Response::error(500, "internal")
        }
    }
}

/// The one field `POST /v1/switch` accepts.
#[derive(serde::Deserialize)]
struct SwitchBody {
    profile: String,
}

/// `POST /v1/switch` — relink the global active profile.
///
/// A thin wrapper over [`switch_profile_noninteractive`], the same action the
/// MCP `switch` tool calls. That is deliberate and load-bearing: the AUTH-1
/// gate (never install credentials a refresh has rejected), the disabled-target
/// refusal, and the divergence policy all live inside it, so this endpoint
/// cannot drift into a weaker switch than the rest of clauth performs.
fn switch(ctx: &ApiContext, req: &Request) -> Response {
    let Ok(parsed) = serde_json::from_slice::<SwitchBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };

    // Resolve to a stored profile BEFORE any mutation — the guard the CLI and
    // MCP paths both apply. Without it an unknown name reaches
    // `link_profile_credentials`, which strips the live credential symlink and
    // creates no replacement, leaving the global session logged out.
    let (canonical, on_divergence) = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        (
            cfg.canonical_name(&parsed.profile),
            cfg.state.default_divergence,
        )
    };
    let Some(canonical) = canonical else {
        return Response::error(404, "profile_not_found");
    };

    // One switch at a time. Without this, a second request parks on the
    // cross-process state flock for its full 25s deadline and then fails
    // anyway; a 409 now is the honest answer.
    let Ok(_gate) = ctx.switch_gate.try_lock() else {
        return Response::error(409, "switch_in_progress");
    };

    match switch_profile_noninteractive(
        &ctx.config,
        &canonical,
        on_divergence,
        oauth::refresh_result,
    ) {
        Ok((previous, active)) => {
            logline!("clauth api: switched to '{active}'");
            Response::json(
                200,
                &serde_json::json!({ "ok": true, "previous": previous, "active": active }),
            )
        }
        Err(e) => {
            let reason = sanitize_for_log(&e.to_string());
            logline!("clauth api: switch to '{canonical}' refused: {reason}");
            // A held state flock is the one retryable failure here: another
            // clauth process is mid-write, and the same request will work in a
            // moment. Everything else needs the operator to change something.
            if e.downcast_ref::<StateLockTimeout>().is_some() {
                Response::refused(503, "state_locked", &reason)
            } else {
                Response::refused(409, "switch_refused", &reason)
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_routes.rs"]
mod tests;
