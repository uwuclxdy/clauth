#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `GET /api/v1/gateway`: the supervisor's published slot, byte for byte, and
//! the same object the status feed carries. The unpaired 401 and the view
//! tier's 200 ride the route-table sweeps in `daemon_api_routes.rs`
//! (`every_route_but_pair_refuses_an_unpaired_caller`,
//! `a_view_device_reads_every_view_route`), which reach every row.

#![cfg(unix)]

use super::*;

use crate::daemon::api::devices::Tier;
use crate::daemon::gateway::{Answerer, GatewayState};
use crate::profile::{AppConfig, AppState};
use crate::testutil::{HomeSandbox, OTHER_TOKEN, body_json, call, req, seed_device};

/// A foreign gateway with every field fixed.
const SLOT_BYTES: &str = r#"{"state":"foreign","config":"/etc/shunt/shunt.toml","binary":"shunt","port":3067,"pid":null,"version":"0.49.1","answerer":"shunt","floor":"0.48.0","restarts":1,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#;

fn slot() -> GatewaySlot {
    GatewaySlot {
        state: GatewayState::Foreign,
        config: Some("/etc/shunt/shunt.toml".to_string()),
        binary: Some("shunt".to_string()),
        port: Some(3067),
        pid: None,
        version: Some("0.49.1".to_string()),
        answerer: Some(Answerer::Shunt),
        floor: "0.48.0".to_string(),
        restarts: 1,
        last_exit: None,
        reason: None,
        since: Some("2026-09-21T14:13:20+00:00".to_string()),
    }
}

#[test]
fn a_view_device_reads_the_published_slot_and_the_feed_carries_the_same_object() {
    let _home = HomeSandbox::new();
    let live = crate::daemon::LiveStores::default();
    *live.gateway.lock().expect("slot") = Some(slot());
    let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }));
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    let ctx = ApiContext::for_tests(
        config,
        status_path,
        Some(live),
        crate::daemon::api::panes::absent_probe(),
    );
    seed_device("phone", Tier::View, OTHER_TOKEN);

    let resp = call(&ctx, &req("GET", "/api/v1/gateway", Some(OTHER_TOKEN), ""));
    assert_eq!(resp.status, 200);
    assert_eq!(
        String::from_utf8(resp.body.clone()).expect("utf-8"),
        SLOT_BYTES
    );

    let feed = call(
        &ctx,
        &req("GET", "/api/v1/status?all=1", Some(OTHER_TOKEN), ""),
    );
    assert_eq!(feed.status, 200);
    assert_eq!(
        body_json(&feed)["gateway"],
        serde_json::from_str::<serde_json::Value>(SLOT_BYTES).expect("fixture")
    );
}
