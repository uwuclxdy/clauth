#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The bridge's route-side pins beyond the socket and hub core: the herdr
//! fallback's exact argv and its none/ambiguous refusals, the listing's
//! `via`/`lacks`/`caps`, and a bind failure leaving the daemon serving with no
//! bridge.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, body_json, call, req, seed_device};

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

fn instance(session_id: &str, proc_start: &str) -> Instance {
    Instance {
        session_id: session_id.to_string(),
        proc_start: proc_start.to_string(),
    }
}

fn table(rows: &[(u32, Instance)]) -> (Table, PeerResolver) {
    let table: Table = Arc::new(Mutex::new(rows.iter().cloned().collect()));
    let read = Arc::clone(&table);
    let resolver: PeerResolver = Box::new(move |pid| read.lock().unwrap().get(&pid).cloned());
    (table, resolver)
}

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn status_path() -> std::path::PathBuf {
    crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json")
}

/// The herdr fallback drives the one joined pane with herdr's exact argv, for
/// both a prompt and an interrupt, when no mod instance is live.
#[test]
fn the_herdr_fallback_drives_the_joined_pane() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_probe = Arc::clone(&seen);
    let probe: PaneProbe = Box::new(move |args, _| {
        seen_probe
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).to_string()).collect());
        match args {
            ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
                success: true,
                stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}}]}}"#.to_vec(),
                stderr: Vec::new(),
            })),
            ["agent", "prompt", "w1:p1", _] | ["pane", "send-keys", "w1:p1", "esc"] => {
                HerdrProbeOut::Ran(Some(HerdrOut {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }))
            }
            _ => HerdrProbeOut::Ran(None),
        }
    });
    let ctx = ApiContext::for_tests_with_bridge(config(), status_path(), probe, Arc::clone(&hub));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"fix it"}"#,
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (200, json!({ "ok": true, "via": "herdr" }))
    );
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/interrupt",
            Some(TOKEN),
            "{}",
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (200, json!({ "ok": true, "via": "herdr" }))
    );

    let seen = seen.lock().unwrap().clone();
    assert!(seen.contains(&vec!["pane".to_string(), "list".to_string()]));
    assert!(seen.contains(&vec![
        "agent".to_string(),
        "prompt".to_string(),
        "w1:p1".to_string(),
        "fix it".to_string()
    ]));
    assert!(seen.contains(&vec![
        "pane".to_string(),
        "send-keys".to_string(),
        "w1:p1".to_string(),
        "esc".to_string()
    ]));
}

/// No joined pane answers 404 `session_not_live` and two answer 409
/// `session_ambiguous`, for both the prompt and the interrupt route.
#[test]
fn the_herdr_fallback_refuses_none_and_ambiguous() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);

    let none_probe: PaneProbe = Box::new(|args, _| match args {
        ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout: br#"{"result":{"panes":[]}}"#.to_vec(),
            stderr: Vec::new(),
        })),
        _ => HerdrProbeOut::Ran(None),
    });
    let ctx =
        ApiContext::for_tests_with_bridge(config(), status_path(), none_probe, Arc::clone(&hub));
    for path in [
        "/api/v1/sessions/s-one/prompt",
        "/api/v1/sessions/s-one/interrupt",
    ] {
        let body = if path.ends_with("prompt") {
            r#"{"text":"x"}"#
        } else {
            "{}"
        };
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(
            (resp.status, body_json(&resp)["error"].clone()),
            (404, json!("session_not_live")),
            "{path}"
        );
    }

    let two_probe: PaneProbe = Box::new(|args, _| {
        match args {
        ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}},{"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t2","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}}]}}"#.to_vec(),
            stderr: Vec::new(),
        })),
        _ => HerdrProbeOut::Ran(None),
    }
    });
    let ctx =
        ApiContext::for_tests_with_bridge(config(), status_path(), two_probe, Arc::clone(&hub));
    for path in [
        "/api/v1/sessions/s-one/prompt",
        "/api/v1/sessions/s-one/interrupt",
    ] {
        let body = if path.ends_with("prompt") {
            r#"{"text":"x"}"#
        } else {
            "{}"
        };
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(
            (resp.status, body_json(&resp)["error"].clone()),
            (409, json!("session_ambiguous")),
            "{path}"
        );
    }
}

/// The listing's `via`/`lacks`/`caps`: a mod-live session, a pane-joined
/// session, and one with neither reach.
#[test]
fn the_listing_flags_mod_and_herdr_reach() {
    let sb = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    for id in ["s-mod", "s-pane", "s-none", "s-both"] {
        let path = sb.home().join(format!(".claude/projects/-w/{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{{\"sessionId\":\"{id}\",\"cwd\":\"/w\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}"
            ),
        )
        .unwrap();
    }

    let (_t, resolver) = table(&[
        (100, instance("s-mod", "1")),
        (200, instance("s-both", "2")),
    ]);
    let hub = Hub::new(true, resolver);
    let start = json!({"session": "s-mod", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true, "caps": ["notes"]});
    assert_eq!(hub.event(100, start.to_string().as_bytes()).0, 200);
    let start_both =
        json!({"session": "s-both", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    assert_eq!(hub.event(200, start_both.to_string().as_bytes()).0, 200);

    let probe: PaneProbe = Box::new(|args, _| {
        match args {
        ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-pane"}},{"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t2","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-both"}}]}}"#.to_vec(),
            stderr: Vec::new(),
        })),
        _ => HerdrProbeOut::Ran(None),
    }
    });
    let ctx = ApiContext::for_tests_with_bridge(config(), status_path(), probe, hub);

    let resp = call(&ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    let rows = body_json(&resp)["sessions"].clone();
    let row = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed"))
            .clone()
    };
    assert_eq!(row("s-mod")["via"], json!(["mod"]));
    assert_eq!(
        row("s-mod")["lacks"],
        json!(["terminal_mirror", "engine_dialogs", "plan_approval"])
    );
    assert_eq!(row("s-pane")["via"], json!(["herdr"]));
    assert_eq!(row("s-pane")["lacks"], json!([]));
    assert_eq!(row("s-none")["via"], json!([]));
    assert_eq!(row("s-none")["lacks"], json!([]));
    assert_eq!(row("s-both")["via"], json!(["herdr", "mod"]));
    assert_eq!(row("s-both")["lacks"], json!([]));

    // `caps` names what the session's live mod declared: none for an older
    // mod, and none without a live mod at all.
    assert_eq!(row("s-mod")["caps"], json!(["notes"]));
    assert_eq!(row("s-both")["caps"], json!([]));
    assert_eq!(row("s-pane")["caps"], json!([]));
    assert_eq!(row("s-none")["caps"], json!([]));
}

/// Two live mod instances on one session id (SB-4) list a capability only
/// when both declare it, whichever declared last: a note could reach either
/// one. An instance whose `end` already owns its final read takes no more
/// answers, so it no longer counts, while the session stays listed by the mod.
#[test]
fn two_instances_on_one_id_list_only_the_caps_both_declare() {
    let sb = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let path = sb.home().join(".claude/projects/-w/s-one.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "{\"sessionId\":\"s-one\",\"cwd\":\"/w\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}",
    )
    .unwrap();

    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-one", "2"))]);
    let hub = Hub::new(true, resolver);
    let older =
        json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    assert_eq!(hub.event(200, older.to_string().as_bytes()).0, 200);
    let capable = json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true, "caps": ["notes"]});
    assert_eq!(hub.event(100, capable.to_string().as_bytes()).0, 200);

    let no_panes: PaneProbe = Box::new(|_, _| HerdrProbeOut::NotInstalled);
    let ctx =
        ApiContext::for_tests_with_bridge(config(), status_path(), no_panes, Arc::clone(&hub));
    let caps = |ctx: &ApiContext| {
        let resp = call(ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
        assert_eq!(resp.status, 200);
        body_json(&resp)["sessions"][0]["caps"].clone()
    };
    assert_eq!(caps(&ctx), json!([]), "one instance lacks notes");

    let reloaded = json!({"session": "s-one", "seq": 2, "kind": "start", "cwd": "/w", "interactive": true, "caps": ["notes"]});
    assert_eq!(hub.event(200, reloaded.to_string().as_bytes()).0, 200);
    assert_eq!(caps(&ctx), json!(["notes"]), "both declare notes");

    let downgraded =
        json!({"session": "s-one", "seq": 3, "kind": "start", "cwd": "/w", "interactive": true});
    assert_eq!(hub.event(200, downgraded.to_string().as_bytes()).0, 200);
    assert_eq!(
        caps(&ctx),
        json!([]),
        "a reload onto an older mod lacks notes"
    );
    hub.inner
        .lock()
        .unwrap()
        .instances
        .get_mut(&200)
        .expect("the older instance")
        .ending = true;
    let resp = call(&ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
    let row = body_json(&resp)["sessions"][0].clone();
    assert_eq!(
        row["caps"],
        json!(["notes"]),
        "an ending instance no longer counts"
    );
    assert_eq!(
        row["via"],
        json!(["mod"]),
        "the session stays listed by the mod"
    );
}

/// A bind failure (a live squatter on the path) leaves `start()` returning no
/// hub, never aborting the daemon.
#[test]
fn a_bind_failure_leaves_the_daemon_without_a_bridge() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _squatter = UnixListener::bind(&path).unwrap();
    assert!(crate::daemon::bridge::start().is_none());
}
