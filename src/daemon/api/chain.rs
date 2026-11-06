//! The fallback-chain mutation routes: order, per-member threshold, and the
//! chain-global wrap-off.
//!
//! Each one is a thin wrapper over the same [`crate::actions`] entry the TUI's
//! Fallback tab calls, so a control device edits exactly the state the machine
//! edits — the same validation, the same persistence, and the same republish of
//! `status.json` before the answer.

use super::http::{Request, Response, flatten_control_chars, sanitize_for_log};
use super::routes::{ApiContext, Caller, ErrorBody, republish};
use crate::actions::{
    ChainEditRefusal, ChainRefusal, set_chain_order, set_member_threshold, set_wrap_off,
};
use crate::lock::StateLockTimeout;
use crate::logline::logline;
use crate::profile::{AppConfig, ProfileName};

/// The one field `POST /api/v1/chain/order` accepts.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct ChainOrderBody {
    members: Vec<String>,
}

/// The answer a successful reorder carries: the canonical member names in the
/// new order.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ChainOrderOk {
    ok: bool,
    members: Vec<String>,
}

/// The fields `POST /api/v1/chain/threshold` accepts.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct ChainThresholdBody {
    profile: String,
    threshold: f64,
}

/// The answer a successful threshold edit carries: the canonical profile name
/// and the stored value.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ChainThresholdOk {
    ok: bool,
    profile: String,
    threshold: f64,
}

/// The one field `POST /api/v1/chain/wrap-off` accepts.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct ChainWrapOffBody {
    wrap_off: bool,
}

/// The answer a successful wrap-off edit carries.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ChainWrapOffOk {
    ok: bool,
    wrap_off: bool,
}

/// `POST /api/v1/chain/order` — replace the fallback chain order.
#[utoipa::path(
    post,
    path = "/api/v1/chain/order",
    request_body = ChainOrderBody,
    responses(
        (status = 200, description = "the new order landed; canonical member names in that order", body = ChainOrderOk),
        (status = 400, description = "the body held no parseable member list (`bad_request`), or the list is not a permutation of the chain (`chain_order_invalid`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 409, description = "a chain edit or a switch is already in flight (`edit_in_progress`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`), or the edit failed (`edit_failed`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn order(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Ok(parsed) = serde_json::from_slice::<ChainOrderBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };

    // Resolve to stored profiles before the gate, the way `switch` resolves its
    // target: an unknown name is refused here with the name in the reason, and
    // the action below only ever sees canonical names.
    let members = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        let mut resolved = Vec::with_capacity(parsed.members.len());
        for raw in &parsed.members {
            let Some(canonical) = cfg.canonical_name(raw) else {
                return Response::refused(
                    400,
                    "chain_order_invalid",
                    &format!("unknown chain member '{}'", flatten_control_chars(raw)),
                );
            };
            resolved.push(ProfileName::from(canonical.as_str()));
        }
        resolved
    };

    // One chain edit at a time, shared with the other two routes and `switch`.
    let Ok(_gate) = ctx.switch_gate.try_lock() else {
        return Response::error(409, "edit_in_progress");
    };

    // The answer and the audit line carry the order that LANDED: a member the
    // fresh roster no longer holds drops out inside the action.
    match run_chain_edit(ctx, caller, "chain order edit", |cfg| {
        set_chain_order(cfg, &members)
    }) {
        Ok(saved) => {
            logline!(
                "clauth api: device '{}' reordered the chain to [{}]",
                caller.device_for_log(),
                saved
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Response::serialize(
                200,
                &ChainOrderOk {
                    ok: true,
                    members: saved.iter().map(|m| m.to_string()).collect(),
                },
            )
        }
        Err(resp) => resp,
    }
}

/// `POST /api/v1/chain/threshold` — set one member's fallback threshold.
#[utoipa::path(
    post,
    path = "/api/v1/chain/threshold",
    request_body = ChainThresholdBody,
    responses(
        (status = 200, description = "the threshold landed; the canonical profile name and the stored value", body = ChainThresholdOk),
        (status = 400, description = "the body held no parseable fields, or the threshold is not finite or outside 0..=100 (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 404, description = "the profile is not stored (`profile_not_found`)", body = ErrorBody),
        (status = 409, description = "a chain edit or a switch is already in flight (`edit_in_progress`), or the profile is not a chain member (`not_a_member`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`), or the edit failed (`edit_failed`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn threshold(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Ok(parsed) = serde_json::from_slice::<ChainThresholdBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };

    let canonical = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = ctx.config.lock().expect("config mutex poisoned");
        cfg.canonical_name(&parsed.profile)
    };
    let Some(canonical) = canonical else {
        return Response::error(404, "profile_not_found");
    };

    let Ok(_gate) = ctx.switch_gate.try_lock() else {
        return Response::error(409, "edit_in_progress");
    };

    let name = ProfileName::from(canonical.as_str());
    match run_chain_edit(ctx, caller, "chain threshold edit", |cfg| {
        set_member_threshold(cfg, &name, parsed.threshold)?;
        Ok(())
    }) {
        Ok(()) => {
            logline!(
                "clauth api: device '{}' set '{}' threshold to {}",
                caller.device_for_log(),
                canonical,
                parsed.threshold
            );
            Response::serialize(
                200,
                &ChainThresholdOk {
                    ok: true,
                    profile: canonical,
                    threshold: parsed.threshold,
                },
            )
        }
        Err(resp) => resp,
    }
}

/// `POST /api/v1/chain/wrap-off` — set the chain-global wrap-off behaviour.
#[utoipa::path(
    post,
    path = "/api/v1/chain/wrap-off",
    request_body = ChainWrapOffBody,
    responses(
        (status = 200, description = "the wrap-off flag landed", body = ChainWrapOffOk),
        (status = 400, description = "the body held no parseable wrap_off (`bad_request`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`), or a view-only device (`control_required`)", body = ErrorBody),
        (status = 409, description = "a chain edit or a switch is already in flight (`edit_in_progress`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`), or the edit failed (`edit_failed`)", body = ErrorBody)
    ),
    security(("bearer" = ["control"]))
)]
pub(crate) fn wrap_off(ctx: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    let Ok(parsed) = serde_json::from_slice::<ChainWrapOffBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };

    let Ok(_gate) = ctx.switch_gate.try_lock() else {
        return Response::error(409, "edit_in_progress");
    };

    match run_chain_edit(ctx, caller, "chain wrap-off edit", |cfg| {
        set_wrap_off(cfg, parsed.wrap_off)?;
        Ok(())
    }) {
        Ok(()) => {
            logline!(
                "clauth api: device '{}' set wrap_off to {}",
                caller.device_for_log(),
                parsed.wrap_off
            );
            Response::serialize(
                200,
                &ChainWrapOffOk {
                    ok: true,
                    wrap_off: parsed.wrap_off,
                },
            )
        }
        Err(resp) => resp,
    }
}

/// Run one chain edit under the config mutex (then the state flock, inside the
/// action), republish on success, and map failures to their closed-set answer.
fn run_chain_edit<T>(
    ctx: &ApiContext,
    caller: &Caller<'_>,
    label: &'static str,
    action: impl FnOnce(&mut AppConfig) -> anyhow::Result<T>,
) -> Result<T, Response> {
    let result = {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let mut cfg = ctx.config.lock().expect("config mutex poisoned");
        action(&mut cfg)
    };
    match result {
        Ok(value) => {
            republish(ctx);
            Ok(value)
        }
        Err(e) => {
            // The open anyhow chain goes to daemon.log, the surface the operator
            // owns; the body keeps only what the closed set reflects.
            logline!(
                "clauth api: device '{}' {label} refused: {}",
                caller.device_for_log(),
                sanitize_for_log(&format!("{e:#}"))
            );
            Err(chain_error_response(&e))
        }
    }
}

/// Map an action failure to its answer. A held state flock is the one retryable
/// failure; an authored refusal carries its own code; any other disk failure is
/// `500 edit_failed`, the same shape `switch` answers with its own `500`.
fn chain_error_response(e: &anyhow::Error) -> Response {
    if let Some(timeout) = e.downcast_ref::<StateLockTimeout>() {
        return Response::refused(
            503,
            "state_locked",
            &flatten_control_chars(&timeout.to_string()),
        );
    }
    if let Some(refusal) = e.downcast_ref::<ChainEditRefusal>() {
        let code = refusal.code.code();
        return match refusal.code {
            ChainRefusal::OrderInvalid => Response::refused(
                400,
                code,
                refusal.reason.as_deref().unwrap_or("invalid chain order"),
            ),
            ChainRefusal::NotAMember => Response::refused(
                409,
                code,
                refusal
                    .reason
                    .as_deref()
                    .unwrap_or("profile not in the fallback chain"),
            ),
            ChainRefusal::BadRequest => Response::error(400, code),
            ChainRefusal::ProfileNotFound => Response::error(404, code),
        };
    }
    Response::refused(500, "edit_failed", "the chain edit failed; see daemon.log")
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_chain.rs"]
mod tests;
