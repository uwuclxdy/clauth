//! `GET /api/v1/proxies` — the managed proxies' slots, the array `status.json`
//! publishes as its `proxies` field. View tier and read-only: no proxy gets a
//! mutating route until a threat-model pass covers remote writes.

use super::http::{Request, Response};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::daemon::proxies::{self, ProxySlot};

#[utoipa::path(
    get,
    path = "/api/v1/proxies",
    responses(
        (status = 200, description = "the managed proxies' slots, one object per registry row, the array the status feed publishes as its proxies field", body = Vec<ProxySlot>),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn proxies(ctx: &ApiContext, _: &Request, _: &Caller<'_>) -> Response {
    let live = ctx.live.as_ref().map(|live| proxies::slots(&live.proxies));
    Response::serialize(200, &proxies::entries(live.as_deref()))
}
