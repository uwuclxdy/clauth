//! `GET /api/v1/gateway` — the managed shunt gateway's slot, the object
//! `status.json` publishes as `gateway`. View tier and read-only: the gateway
//! gets no mutating route until a threat-model pass covers remote writes of
//! its config and its admin token.

use super::http::{Request, Response};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::daemon::gateway::{GatewaySlot, published, slot_or_record};

#[utoipa::path(
    get,
    path = "/api/v1/gateway",
    responses(
        (status = 200, description = "the managed gateway's slot, the object the status feed publishes as its gateway field", body = GatewaySlot),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn gateway(ctx: &ApiContext, _: &Request, _: &Caller<'_>) -> Response {
    let published = ctx.live.as_ref().and_then(|live| published(&live.gateway));
    Response::serialize(200, &slot_or_record(published.as_ref()))
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_gateway.rs"]
mod tests;
