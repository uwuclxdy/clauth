//! Route table: read the feed, switch the account, clone the accounts. Nothing
//! else.
//!
//! The narrow surface is the design. A switch is the one mutation the daemon
//! already performs unattended, so exposing it adds no capability the fallback
//! chain does not have; everything that needs a human — a diverged live login,
//! an unprovable identity — is refused here exactly as it is refused for the
//! scheduler and the MCP tool, and the TUI stays the only place to resolve it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sha2::Digest as _;

use crate::actions::switch_profile_noninteractive;
use crate::daemon::build_status;
use crate::lock::StateLockTimeout;
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::oauth;
use crate::profile::ConfigHandle;

use super::http::{Request, Response, sanitize_for_log};
use super::token::AuthToken;

/// Every route lives under this prefix, and it is spelled once.
///
/// `/api/` keeps the daemon's own surface out of the way of anything a reverse
/// proxy in front of it may want to own, and the version segment is what makes a
/// breaking schema change additive: `/api/v2/status` can be served beside this
/// one rather than replacing it. Bumping it is a one-line change here, which is
/// the point of the constant — the route table below matches on the remainder.
pub(crate) const API_PREFIX: &str = "/api/v1";

/// Everything a request handler is allowed to touch.
pub(crate) struct ApiContext {
    pub(crate) config: ConfigHandle,
    /// `~/.clauth/status.json` — the feed the main loop rewrites each tick.
    pub(crate) status_path: PathBuf,
    pub(crate) token: AuthToken,
    /// One in-flight `POST /api/v1/switch` at a time. See [`rank::ApiSwitch`].
    pub(crate) switch_gate: RankedMutex<(), rank::ApiSwitch>,
    /// The scheduler's in-memory signals, when a daemon built this context.
    ///
    /// Every route that BUILDS a body rather than serving the published file
    /// needs them, or it answers with `fetch_status`, `next_refresh_at`, `stale`
    /// and `pending_switch` derived from a file mtime while the plain route,
    /// reading the file the scheduler wrote, carries the real ones. `None` only
    /// where there is no scheduler to ask — the tests that exercise a route
    /// without a daemon behind it.
    pub(crate) live: Option<crate::daemon::LiveStores>,
}

impl ApiContext {
    pub(crate) fn new(
        config: ConfigHandle,
        status_path: PathBuf,
        token: AuthToken,
        live: Option<crate::daemon::LiveStores>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            status_path,
            token,
            switch_gate: RankedMutex::new(()),
            live,
        })
    }
}

/// Authenticate, then dispatch. Every route requires the bearer token,
/// including health: an unauthenticated caller still learns the daemon is alive
/// (it answers 401 rather than refusing the connection), which is all a liveness
/// probe needs, and nothing else leaks — not the version, not an account name.
pub(crate) fn handle(ctx: &ApiContext, req: &Request) -> Response {
    // Against the token as it is on disk NOW, not the one captured at spawn, so
    // `clauth daemon --rotate-token` takes effect against a running daemon.
    let live = crate::daemon::api::token::current_or(&ctx.token);
    if !req.bearer.as_deref().is_some_and(|t| live.verify(t)) {
        return Response::unauthorized();
    }
    // Anything outside the prefix is 404 before the table is consulted, so the
    // table itself never repeats the prefix and cannot drift from it.
    let Some(route) = req.path.strip_prefix(API_PREFIX) else {
        return Response::error(404, "not_found");
    };
    match (req.method.as_str(), route) {
        ("GET", "/health") => health(),
        ("GET", "/status") => status(ctx, req),
        ("POST", "/switch") => switch(ctx, req),
        // A known path reached with the wrong method is 405, so a client with a
        // typo'd verb gets told which half is wrong.
        (_, "/health" | "/status") => Response::error(405, "method_not_allowed"),
        (_, "/switch") => Response::error(405, "method_not_allowed"),
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

/// `GET /api/v1/status` — the same body `~/.clauth/status.json` carries.
///
/// Served straight off disk in the common case. The main loop already rewrites
/// that file atomically every tick, so passing the bytes through takes no lock,
/// duplicates no serialization, and cannot drift from the documented schema.
/// `?all=1` and a missing file both fall back to building a body here, which is
/// the same `build_status` the file itself came from.
///
/// Conditional, and optionally BLOCKING: `?wait=` with a matching
/// `If-None-Match` holds the request open until the feed's content actually
/// changes. That is what turns a client's account display from
/// "correct within its poll interval" into "correct within a round trip", and
/// with `POST /api/v1/switch` republishing the file itself, a switch made through
/// the API wakes every waiting reader immediately.
///
/// `?all=1` never waits: it builds its body from config rather than the file,
/// so there is no file to watch for it.
fn status(ctx: &ApiContext, req: &Request) -> Response {
    let include_disabled = req.flag("all");
    if !include_disabled {
        let waited = req
            .param("wait")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs.min(MAX_WAIT_SECS)));
        // Only a client that says what it already holds can be made to wait;
        // with no tag every answer is a change, so there is nothing to wait for.
        if let (Some(wait), Some(tag)) = (waited, req.if_none_match.as_deref()) {
            return wait_for_status_change(ctx, tag, wait);
        }
        if let Ok(body) = std::fs::read(&ctx.status_path) {
            let etag = etag_for(&body);
            if req.if_none_match.as_deref() == Some(etag.as_str()) {
                return Response::not_modified(etag);
            }
            return Response::raw_json_tagged(200, body, etag);
        }
    }
    // No file yet (the daemon is still in its first tick), or the caller asked
    // for the disabled accounts the published feed always hides.
    //
    // The live stores are snapshotted FIRST and their locks released inside
    // `snapshot`, so nothing below holds one when CONFIG — which outranks every
    // one of them — is taken next.
    let live = ctx.live.as_ref().map(crate::daemon::LiveStores::snapshot);
    let (snapshot, interval) = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        (cfg.clone(), cfg.state.refresh_interval_ms)
    };
    let body = build_status(
        &snapshot,
        interval,
        live.as_ref()
            .map(crate::daemon::LiveSnapshot::signals)
            .as_ref(),
        include_disabled,
    );
    match serde_json::to_vec(&body) {
        Ok(bytes) => Response::raw_json(200, bytes),
        Err(e) => {
            logline!("clauth api: failed to serialize a status body: {e}");
            Response::error(500, "internal")
        }
    }
}

/// Block until `status.json`'s content leaves `tag`, or `wait` elapses.
///
/// Content, not mtime. The feed is a single small file, so re-reading and
/// digesting it every [`WAIT_POLL`] costs almost nothing — and unlike an mtime
/// it cannot be fooled. (A filesystem that stamps two writes microseconds apart
/// with one mtime is not hypothetical; comparing content is immune to it.)
fn wait_for_status_change(ctx: &ApiContext, tag: &str, wait: Duration) -> Response {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Response::not_modified(tag.to_string());
        }
        std::thread::sleep(WAIT_POLL.min(deadline - now));

        // A file caught mid-replacement is not this request's problem: keep
        // waiting rather than handing the client an error to interpret.
        let Ok(body) = std::fs::read(&ctx.status_path) else {
            continue;
        };
        let etag = etag_for(&body);
        if etag == tag {
            continue;
        }
        return Response::raw_json_tagged(200, body, etag);
    }
}

/// The feed's entity tag: a digest of everything in the body a reader could act
/// on, so it changes when and only when they would see something different.
/// Quoted, as HTTP wants.
///
/// `generated_at` is excluded, and that exclusion is the difference between a
/// long poll and a one-second one: the main loop rewrites the feed every tick,
/// and on a quiet system that stamp is the ONLY field that moves. Digesting it
/// would wake every waiting reader once a second to hand them a body identical
/// in every respect they care about. `MirrorBody::etag` leaves its own timestamp
/// out for the same reason.
///
/// A body that will not parse is digested whole. That is the safe direction: a
/// tag that changes too often costs a wakeup, while one that changes too rarely
/// leaves a reader showing an account the operator has already left.
fn etag_for(body: &[u8]) -> String {
    let meaningful = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|mut value| {
            value.as_object_mut()?.remove("generated_at");
            serde_json::to_vec(&value).ok()
        });
    let bytes = meaningful.as_deref().unwrap_or(body);
    format!(
        "\"{}\"",
        hex::encode(<[u8; 32]>::from(sha2::Sha256::digest(bytes)))
    )
}

/// Longest a `?wait` may hold a connection. Comfortably inside `Limits`'
/// 120-second connection lifetime, so a waiting request always gets to answer on
/// the connection it arrived on.
const MAX_WAIT_SECS: u64 = 60;

/// How often a wait re-checks. Short enough that a switch reads as instant.
const WAIT_POLL: Duration = Duration::from_millis(250);

/// The one field `POST /api/v1/switch` accepts.
#[derive(serde::Deserialize)]
struct SwitchBody {
    profile: String,
}

/// `POST /api/v1/switch` — relink the global active profile.
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
        &crate::profile::ProfileName::from(canonical.as_str()),
        on_divergence,
        oauth::refresh_result,
    ) {
        Ok((previous, active)) => {
            logline!("clauth api: switched to '{active}'");
            // Republish NOW rather than leaving it to the next scheduler tick.
            // Every `GET /api/v1/status?wait=` is parked on this file's content, and
            // this is the daemon's own switch, so there is no other owner to
            // defer to. Cloned out of the mutex first: `write_status_feed`
            // stats and reads every profile's caches, which has no business
            // running under the config lock.
            let live = ctx.live.as_ref().map(crate::daemon::LiveStores::snapshot);
            let snapshot = {
                #[allow(
                    clippy::expect_used,
                    reason = "config mutex poisoning is unrecoverable"
                )]
                let cfg = ctx.config.lock().expect("config mutex poisoned");
                cfg.clone()
            };
            crate::daemon::write_status_feed(
                &snapshot,
                live.as_ref()
                    .map(crate::daemon::LiveSnapshot::signals)
                    .as_ref(),
            );
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
