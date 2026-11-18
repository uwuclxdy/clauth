//! The Web Push stores behind the routes: the VAPID key and the subscription
//! list are owner-only, a key file that does not parse is never minted over,
//! no surface carries the private key, an auth secret or an endpoint, and a
//! revoke whose push row cannot be dropped still removes the device.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use super::*;

use crate::daemon::api::devices::{self, Tier};
use crate::daemon::api::routes::{ApiContext, republish};
use crate::profile::{AppConfig, AppState};
use crate::testutil::{
    HomeSandbox, OTHER_TOKEN, body_json, call, ctx_with, peer, req, seed_device,
};

/// RFC 8291's example browser key, a valid uncompressed P-256 point.
const P256DH: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
/// 16 bytes no other fixture holds: `push-auth-canary` in ASCII.
const AUTH: &str = "cHVzaC1hdXRoLWNhbmFyeQ";
/// An endpoint path segment no other fixture holds.
const ENDPOINT_MARK: &str = "endpoint-canary-7f3a";

fn ctx() -> std::sync::Arc<ApiContext> {
    let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }));
    let ctx = ctx_with(config);
    seed_device("phone", Tier::View, OTHER_TOKEN);
    ctx
}

fn subscription(endpoint: &str, p256dh: &str, auth: &str) -> String {
    serde_json::json!({
        "endpoint": endpoint,
        "keys": {"p256dh": p256dh, "auth": auth},
    })
    .to_string()
}

fn put(ctx: &ApiContext, body: &str) -> Response {
    call(
        ctx,
        &req("PUT", "/api/v1/push/subscription", Some(OTHER_TOKEN), body),
    )
}

fn get_key(ctx: &ApiContext) -> Response {
    call(ctx, &req("GET", "/api/v1/push/key", Some(OTHER_TOKEN), ""))
}

fn endpoint() -> String {
    format!("https://fcm.googleapis.com/fcm/send/{ENDPOINT_MARK}")
}

#[test]
fn the_key_and_the_subscription_list_are_owner_only() {
    let home = HomeSandbox::new();
    let ctx = ctx();

    assert_eq!(get_key(&ctx).status, 200);
    assert_eq!(
        put(&ctx, &subscription(&endpoint(), P256DH, AUTH)).status,
        200
    );

    let dir = home.home().join(".clauth");
    for file in [KEY_FILE, STORE_FILE] {
        let path = dir.join(file);
        assert!(path.is_file(), "{file} was written");
        assert_eq!(
            crate::testutil::owner_only_violations(&path),
            Vec::<String>::new(),
            "{file}"
        );
    }
}

#[test]
fn a_key_file_that_does_not_parse_is_never_minted_over() {
    let home = HomeSandbox::new();
    let ctx = ctx();
    let path = home.home().join(".clauth").join(KEY_FILE);
    crate::profile::atomic_write_600(&path, b"not a key").expect("plant a bad key");

    let resp = get_key(&ctx);

    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (500, serde_json::json!("internal"))
    );
    assert_eq!(std::fs::read(&path).expect("key file"), b"not a key");
}

/// The flush after the atomic write is a strict part of both stores: a
/// directory sync that fails fails the subscription write and the key mint.
#[test]
fn a_failed_directory_sync_fails_the_subscription_write_and_the_key_mint() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    let mode = std::fs::metadata(&dir).expect("mode").permissions();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).expect("drop read");
    let written = with_state_lock(|held| write_store(held, &Store::empty()));
    let minted = vapid_key();
    std::fs::set_permissions(&dir, mode).expect("restore");
    let err = written.expect_err("the directory sync's failure fails the subscription write");
    assert!(format!("{err:#}").contains("failed to flush"), "{err:#}");
    let err = minted.expect_err("the directory sync's failure fails the key mint");
    assert!(format!("{err:#}").contains("failed to flush"), "{err:#}");
}

#[test]
fn an_endpoint_on_another_port_or_a_dotted_host_is_refused() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    for endpoint in [
        "https://fcm.googleapis.com:8443/fcm/send/abc",
        "https://fcm.googleapis.com./fcm/send/abc",
    ] {
        let resp = put(&ctx, &subscription(endpoint, P256DH, AUTH));
        assert_eq!(
            (resp.status, body_json(&resp)["error"].clone()),
            (400, serde_json::json!("push_endpoint_not_allowed")),
            "{endpoint}"
        );
    }
}

#[test]
fn keys_of_the_wrong_size_or_shape_are_refused() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    for (case, p256dh, auth) in [
        // 64 bytes: the point with its last byte cut.
        (
            "short point",
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw",
            AUTH,
        ),
        // 65 bytes opening 0x02 rather than 0x04.
        (
            "compressed tag",
            "AiVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            AUTH,
        ),
        // The RFC point with its last byte flipped: 65 bytes opening 0x04,
        // off the curve.
        (
            "off-curve point",
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw8",
            AUTH,
        ),
        // 15 bytes: `push-auth-canar`.
        ("short auth", P256DH, "cHVzaC1hdXRoLWNhbmFy"),
        ("not base64", P256DH, "!!!!"),
    ] {
        let resp = put(&ctx, &subscription(&endpoint(), p256dh, auth));
        assert_eq!(
            (resp.status, body_json(&resp)["error"].clone()),
            (400, serde_json::json!("bad_request")),
            "{case}"
        );
    }
}

/// The 32-byte private scalar inside ring's P-256 PKCS#8 document: the
/// `ECPrivateKey` version (`02 01 01`) then the scalar's `04 20` header.
fn private_scalar(pkcs8: &[u8]) -> Vec<u8> {
    let header = [0x02, 0x01, 0x01, 0x04, 0x20];
    let at = pkcs8
        .windows(header.len())
        .position(|window| window == header)
        .expect("the PKCS#8 document carries an ECPrivateKey scalar")
        + header.len();
    pkcs8[at..at + 32].to_vec()
}

/// Every spelling a secret could take on a surface: raw, hex, base64 and
/// base64url.
fn spellings(secret: &[u8]) -> Vec<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    vec![
        secret.to_vec(),
        hex::encode(secret).into_bytes(),
        STANDARD_NO_PAD.encode(secret).into_bytes(),
        URL_SAFE_NO_PAD.encode(secret).into_bytes(),
    ]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn no_surface_carries_the_private_key_an_auth_secret_or_an_endpoint() {
    let home = HomeSandbox::new();
    let ctx = ctx();
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();

    let mut bodies = vec![
        get_key(&ctx).body,
        put(&ctx, &subscription(&endpoint(), P256DH, AUTH)).body,
    ];
    // A refused PUT carrying the same secrets, and a drop then re-register.
    bodies.push(
        put(
            &ctx,
            &subscription("http://fcm.googleapis.com/x", P256DH, AUTH),
        )
        .body,
    );
    bodies.push(
        call(
            &ctx,
            &req("DELETE", "/api/v1/push/subscription", Some(OTHER_TOKEN), ""),
        )
        .body,
    );
    bodies.push(put(&ctx, &subscription(&endpoint(), P256DH, AUTH)).body);
    republish(&ctx);
    drop(capture);

    let logged = lines.snapshot();
    assert!(
        logged
            .iter()
            .any(|line| line.contains("registered a push subscription")),
        "the capture took the route's lines: {logged:?}"
    );
    let status = std::fs::read(&ctx.status_path).expect("the republish wrote status.json");
    let dir = home.home().join(".clauth");
    let store = read_store().expect("the store reads");
    let debugs = format!(
        "{:?} {:?}",
        store.subscriptions,
        vapid_key().expect("the key reads")
    );

    let scalar = private_scalar(&std::fs::read(dir.join(KEY_FILE)).expect("key file"));
    let auth = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(AUTH)
        .expect("fixture auth");
    let mut secrets: Vec<(&str, Vec<u8>)> = Vec::new();
    for spelling in spellings(&scalar) {
        secrets.push(("private key", spelling));
    }
    for spelling in spellings(&auth) {
        secrets.push(("auth", spelling));
    }
    secrets.push(("endpoint", ENDPOINT_MARK.as_bytes().to_vec()));

    let mut surfaces: Vec<(String, Vec<u8>)> = bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| (format!("response {i}"), body))
        .collect();
    surfaces.push(("status.json".to_string(), status));
    surfaces.push(("log".to_string(), logged.join("\n").into_bytes()));
    surfaces.push(("Debug".to_string(), debugs.into_bytes()));

    for (surface, haystack) in &surfaces {
        for (secret, needle) in &secrets {
            assert!(
                !contains(haystack, needle),
                "{surface} carries the {secret}"
            );
        }
    }
}

#[test]
fn a_revoke_whose_push_row_cannot_be_dropped_still_removes_the_device() {
    let home = HomeSandbox::new();
    let ctx = ctx();
    assert_eq!(
        put(&ctx, &subscription(&endpoint(), P256DH, AUTH)).status,
        200
    );
    std::fs::write(home.home().join(".clauth").join(STORE_FILE), b"{ not json")
        .expect("damage the subscription list");

    let err = devices::revoke("phone").expect_err("the push row could not be dropped");

    let left = err
        .downcast_ref::<SubscriptionLeft>()
        .expect("the error names the row it left");
    assert_eq!(left.device, "phone");
    assert!(
        devices::authenticate(Some(OTHER_TOKEN))
            .expect("the device list reads")
            .is_none(),
        "the device is revoked"
    );
}

#[test]
fn a_repeated_event_is_listed_once() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let body = serde_json::json!({
        "endpoint": endpoint(),
        "keys": {"p256dh": P256DH, "auth": AUTH},
        "events": ["account", "blocked", "account"],
    })
    .to_string();

    let resp = put(&ctx, &body);

    assert_eq!(
        body_json(&resp)["events"],
        serde_json::json!(["blocked", "account"])
    );
}

const TABLET_TOKEN: &str = "abababababababababababababababababababababababababababababababab";

fn delete_as(ctx: &ApiContext, token: &str) -> u16 {
    call(
        ctx,
        &req("DELETE", "/api/v1/push/subscription", Some(token), ""),
    )
    .status
}

/// Each device writes, drops and loses on revoke only its own row.
#[test]
fn a_device_touches_only_its_own_row() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    seed_device("tablet", Tier::View, TABLET_TOKEN);
    let put_as = |token: &str, path: &str| {
        call(
            &ctx,
            &req(
                "PUT",
                "/api/v1/push/subscription",
                Some(token),
                &subscription(
                    &format!("https://fcm.googleapis.com/fcm/send/{path}"),
                    P256DH,
                    AUTH,
                ),
            ),
        )
        .status
    };

    let registered = [put_as(OTHER_TOKEN, "phone"), put_as(TABLET_TOKEN, "tablet")];
    let phone_drop = delete_as(&ctx, OTHER_TOKEN);
    let tablet_kept = read_store()
        .expect("store")
        .subscriptions
        .iter()
        .map(|row| (row.device.clone(), row.endpoint.clone()))
        .collect::<Vec<_>>();
    let re_registered = put_as(OTHER_TOKEN, "phone-2");
    devices::revoke("phone").expect("revoke the phone");
    let after_revoke = read_store()
        .expect("store")
        .subscriptions
        .iter()
        .map(|row| row.device.clone())
        .collect::<Vec<_>>();
    seed_device("phone", Tier::View, OTHER_TOKEN);
    let repaired_drop = delete_as(&ctx, OTHER_TOKEN);

    assert_eq!(
        (
            registered,
            phone_drop,
            tablet_kept,
            re_registered,
            after_revoke,
            repaired_drop
        ),
        (
            [200, 200],
            200,
            vec![(
                "tablet".to_string(),
                "https://fcm.googleapis.com/fcm/send/tablet".to_string()
            )],
            200,
            vec!["tablet".to_string()],
            404,
        )
    );
}

/// A device revoked while its PUT waited on the state flock writes no row:
/// the pairing is checked again under the hold.
#[test]
fn a_put_from_a_pairing_revoked_since_it_authenticated_writes_nothing() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let device = devices::authenticate(Some(OTHER_TOKEN))
        .expect("the device list reads")
        .expect("the phone authenticates");
    devices::revoke("phone").expect("revoke the phone");

    let resp = subscribe(
        &ctx,
        &req(
            "PUT",
            "/api/v1/push/subscription",
            Some(OTHER_TOKEN),
            &subscription(&endpoint(), P256DH, AUTH),
        ),
        &crate::daemon::api::routes::Caller {
            peer: peer(),
            device: Some(&device),
            target: None,
            ask: None,
        },
    );

    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (401, serde_json::json!("unauthorized"))
    );
    assert!(read_store().expect("store").subscriptions.is_empty());
}

/// Pose the row a failed revoke leaves: `device`'s row under a pairing the
/// device list no longer holds.
fn orphan(device: &str) {
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join(STORE_FILE);
    let mut store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("store")).expect("store json");
    for row in store["subscriptions"].as_array_mut().expect("rows") {
        if row["device"] == device {
            row["pairing"] = "a-pairing-no-device-holds".into();
        }
    }
    std::fs::write(&path, serde_json::to_vec(&store).expect("encode")).expect("write store");
}

fn devices_with_rows() -> Vec<String> {
    read_store()
        .expect("store")
        .subscriptions
        .into_iter()
        .map(|row| row.device)
        .collect()
}

/// Every write keeps only rows whose pairing the device list still holds, so
/// a row a failed revoke left goes with the next PUT or DELETE.
#[test]
fn a_write_drops_rows_whose_pairing_the_device_list_no_longer_holds() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    seed_device("tablet", Tier::View, TABLET_TOKEN);
    let put_as = |token: &str| {
        call(
            &ctx,
            &req(
                "PUT",
                "/api/v1/push/subscription",
                Some(token),
                &subscription(&endpoint(), P256DH, AUTH),
            ),
        )
        .status
    };
    assert_eq!([put_as(OTHER_TOKEN), put_as(TABLET_TOKEN)], [200, 200]);

    orphan("phone");
    assert_eq!(put_as(TABLET_TOKEN), 200);
    let after_put = devices_with_rows();

    assert_eq!(put_as(OTHER_TOKEN), 200);
    orphan("tablet");
    assert_eq!(delete_as(&ctx, OTHER_TOKEN), 200);
    let after_delete = devices_with_rows();

    assert_eq!(
        (after_put, after_delete),
        (vec!["tablet".to_string()], Vec::<String>::new())
    );
}

/// A DELETE from a pairing revoked since it authenticated drops nothing,
/// whatever row the name holds now.
#[test]
fn a_delete_from_a_pairing_revoked_since_it_authenticated_drops_nothing() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let old = devices::authenticate(Some(OTHER_TOKEN))
        .expect("the device list reads")
        .expect("the phone authenticates");
    devices::revoke("phone").expect("revoke the phone");
    seed_device("phone", Tier::View, TABLET_TOKEN);
    assert_eq!(
        call(
            &ctx,
            &req(
                "PUT",
                "/api/v1/push/subscription",
                Some(TABLET_TOKEN),
                &subscription(&endpoint(), P256DH, AUTH),
            ),
        )
        .status,
        200
    );

    let resp = unsubscribe(
        &ctx,
        &req("DELETE", "/api/v1/push/subscription", Some(OTHER_TOKEN), ""),
        &crate::daemon::api::routes::Caller {
            peer: peer(),
            device: Some(&old),
            target: None,
            ask: None,
        },
    );

    assert_eq!(
        (resp.status, devices_with_rows()),
        (401, vec!["phone".to_string()])
    );
}

/// A string where the `schema` number belongs would otherwise quote it in the
/// parser's message; the read names the position alone.
#[test]
fn a_mistyped_push_store_names_the_position_and_never_the_secret() {
    let home = HomeSandbox::new();
    let path = home.home().join(".clauth").join("push-subscriptions.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir .clauth");
    std::fs::write(&path, "{\"schema\": \"sk-c2probe-NOTAKEY\"}").expect("mistyped store");
    let err = match read_store() {
        Err(e) => e,
        Ok(_) => panic!("a mistyped store must fail the read"),
    };
    let rendered = format!("{err:#}");
    assert_eq!(
        rendered,
        format!(
            "{} is not a push subscription list clauth can read (line 1, column 31)",
            path.display()
        )
    );
    assert!(
        err.source().is_none(),
        "the parse error must not ride as a source: {err:?}"
    );
}
